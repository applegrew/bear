// ---------------------------------------------------------------------------
// Bear Browser Client — xterm.js powered terminal connecting to bear-server
// ---------------------------------------------------------------------------

const SERVER_URL = 'http://127.0.0.1:49321';
const WS_BASE   = 'ws://127.0.0.1:49321';

// ANSI color helpers
const C = {
  reset:   '\x1b[0m',
  bold:    '\x1b[1m',
  dim:     '\x1b[2m',
  italic:  '\x1b[3m',
  blue:    '\x1b[38;5;111m',
  green:   '\x1b[38;5;114m',
  yellow:  '\x1b[38;5;180m',
  red:     '\x1b[38;5;204m',
  magenta: '\x1b[38;5;141m',
  cyan:    '\x1b[38;5;80m',
  gray:    '\x1b[38;5;102m',
  white:   '\x1b[38;5;252m',
};

const PROMPT = `${C.bold}${C.blue}bear> ${C.reset}`;

// ---------------------------------------------------------------------------
// BearClient class
// ---------------------------------------------------------------------------

export class BearClient {
  constructor(term, fitAddon) {
    this.term = term;
    this.fitAddon = fitAddon;
    this.ws = null;
    this.sessionId = null;

    // Input state
    this.inputBuf = '';
    this.cursorPos = 0;
    this.history = [];
    this.historyIdx = -1;
    this.savedInput = '';

    // Tool confirmation state
    this.pendingToolCall = null;
    this.autoApproved = new Set();

    // Session picker state
    this.inSessionPicker = false;
    this.pickerSessions = [];
    this.pickerIdx = 0;
    this.pickerRendered = false;

    this._bindTerminal();
  }

  // -------------------------------------------------------------------------
  // Boot
  // -------------------------------------------------------------------------

  async boot() {
    this._printBanner();
    try {
      await fetch(SERVER_URL + '/sessions');
    } catch {
      this._writeln(`${C.red}Cannot reach bear-server at ${SERVER_URL}${C.reset}`);
      this._writeln(`${C.red}Start it with: cargo run -p bear-server${C.reset}`);
      this._drawPrompt();
      return;
    }
    await this._showSessionPicker();
  }

  // -------------------------------------------------------------------------
  // Banner
  // -------------------------------------------------------------------------

  _printBanner() {
    this.term.writeln('');
    this.term.writeln(`${C.bold}${C.blue}  ╔══════════════════════════════════╗${C.reset}`);
    this.term.writeln(`${C.bold}${C.blue}  ║${C.reset}${C.bold}   🐻 Bear — Browser Terminal     ${C.blue}║${C.reset}`);
    this.term.writeln(`${C.bold}${C.blue}  ╚══════════════════════════════════╝${C.reset}`);
    this.term.writeln('');
  }

  // -------------------------------------------------------------------------
  // Session picker (rendered in terminal)
  // -------------------------------------------------------------------------

  async _showSessionPicker() {
    this.inSessionPicker = true;
    this.pickerIdx = 0;

    try {
      const res = await fetch(SERVER_URL + '/sessions');
      const data = await res.json();
      this.pickerSessions = data.sessions || [];
    } catch {
      this.pickerSessions = [];
    }

    this._writeln(`${C.bold}${C.white}  Select a session:${C.reset}`);
    this._writeln('');

    this.pickerRendered = false;
    this._renderPicker();
  }

  _renderPicker() {
    const items = [
      { label: '+ New Session', detail: '' },
      ...this.pickerSessions.map(s => ({
        label: s.id.substring(0, 8) + '…',
        detail: s.cwd,
      })),
    ];

    // +1 for the hint line at the bottom
    const totalLines = items.length + 1;

    // Clear previous render (skip on first draw)
    if (this.pickerRendered) {
      for (let i = 0; i < totalLines; i++) {
        this.term.write('\x1b[A\x1b[2K');
      }
    }
    this.pickerRendered = true;

    for (let i = 0; i < items.length; i++) {
      const selected = i === this.pickerIdx;
      const prefix = selected ? `${C.bold}${C.blue}  ❯ ` : `${C.gray}    `;
      const labelColor = selected ? C.white : C.gray;
      const detailStr = items[i].detail ? `  ${C.dim}${C.gray}${items[i].detail}${C.reset}` : '';
      this.term.writeln(`${prefix}${labelColor}${items[i].label}${C.reset}${detailStr}`);
    }

    this.term.writeln(`${C.gray}  ↑/↓ navigate, Enter select${C.reset}`);
  }

  async _pickerSelect() {
    this.inSessionPicker = false;
    this.term.writeln('');

    if (this.pickerIdx === 0) {
      // New session
      this._writeln(`${C.gray}  Creating new session…${C.reset}`);
      try {
        const res = await fetch(SERVER_URL + '/sessions', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ cwd: null }),
        });
        const data = await res.json();
        this._connectToSession(data.session.id);
      } catch (e) {
        this._writeln(`${C.red}  Failed to create session: ${e.message}${C.reset}`);
        this._drawPrompt();
      }
    } else {
      const session = this.pickerSessions[this.pickerIdx - 1];
      this._connectToSession(session.id);
    }
  }

  // -------------------------------------------------------------------------
  // WebSocket connection
  // -------------------------------------------------------------------------

  _connectToSession(sid) {
    this.sessionId = sid;
    this.pendingToolCall = null;
    this.autoApproved.clear();

    if (this.ws) { this.ws.close(); this.ws = null; }

    this._writeln(`${C.gray}  Connecting…${C.reset}`);

    this.ws = new WebSocket(WS_BASE + '/ws/' + sid);

    this.ws.onopen = () => {
      // prompt will be drawn after session_info message
    };

    this.ws.onclose = () => {
      this._writeln(`${C.gray}  Disconnected.${C.reset}`);
    };

    this.ws.onerror = () => {
      this._writeln(`${C.red}  WebSocket error. Is bear-server running?${C.reset}`);
    };

    this.ws.onmessage = (event) => {
      let msg;
      try { msg = JSON.parse(event.data); } catch { return; }
      this._handleServerMessage(msg);
    };
  }

  // -------------------------------------------------------------------------
  // Server message dispatch
  // -------------------------------------------------------------------------

  _handleServerMessage(msg) {
    switch (msg.type) {
      case 'session_info':
        this._writeln(`${C.green}  Connected to session ${C.bold}${msg.session.id}${C.reset}`);
        this._writeln(`${C.gray}  Working directory: ${msg.session.cwd}${C.reset}`);
        this._writeln(`${C.gray}  Type /help for commands${C.reset}`);
        this.term.writeln('');
        this._drawPrompt();
        break;

      case 'assistant_text':
        this._clearInputLine();
        for (const line of msg.text.split('\n')) {
          this._writeln(`${C.green}  ${line}${C.reset}`);
        }
        this._drawPrompt();
        break;

      case 'tool_request': {
        const tc = msg.tool_call;
        const baseCmd = this._extractBaseCommand(tc);

        this._clearInputLine();

        if (this.autoApproved.has(baseCmd)) {
          const preview = JSON.stringify(tc.arguments).substring(0, 80);
          this._writeln(`${C.gray}  [auto-approved] ${tc.name} ${preview}${C.reset}`);
          this._sendJson({ type: 'tool_confirm', tool_call_id: tc.id, approved: true });
          this._drawPrompt();
        } else {
          this.pendingToolCall = tc;
          const argsStr = JSON.stringify(tc.arguments, null, 2);
          this._writeln(`${C.bold}${C.yellow}  [tool] ${tc.name}${C.reset}`);
          for (const line of argsStr.split('\n')) {
            this._writeln(`${C.yellow}    ${line}${C.reset}`);
          }
          this._writeln(`${C.white}  Allow? ${C.green}[y]es ${C.red}[n]o ${C.yellow}[a]lways${C.reset}`);
          this._drawConfirmPrompt();
        }
        break;
      }

      case 'tool_output':
        this._clearInputLine();
        for (const line of msg.output.split('\n')) {
          this._writeln(`${C.cyan}  │ ${line}${C.reset}`);
        }
        this._drawPrompt();
        break;

      case 'process_started':
        this._clearInputLine();
        this._writeln(`${C.magenta}  [proc] Started pid=${msg.info.pid} cmd=${msg.info.command}${C.reset}`);
        this._drawPrompt();
        break;

      case 'process_output':
        this._clearInputLine();
        this._writeln(`${C.magenta}  [${msg.pid}] ${msg.text}${C.reset}`);
        this._drawPrompt();
        break;

      case 'process_exited': {
        const code = msg.code !== null && msg.code !== undefined ? msg.code : 'unknown';
        this._clearInputLine();
        this._writeln(`${C.magenta}  [proc] Process ${msg.pid} exited (code ${code})${C.reset}`);
        this._drawPrompt();
        break;
      }

      case 'process_list_result':
        this._clearInputLine();
        if (msg.processes.length === 0) {
          this._writeln(`${C.gray}  No background processes.${C.reset}`);
        } else {
          this._writeln(`${C.white}  Background processes:${C.reset}`);
          for (const p of msg.processes) {
            const status = p.running ? 'running' : 'exited';
            this._writeln(`${C.gray}    pid=${p.pid} [${status}] ${p.command}${C.reset}`);
          }
        }
        this._drawPrompt();
        break;

      case 'notice':
        this._clearInputLine();
        this._writeln(`${C.gray}  ${msg.text}${C.reset}`);
        this._drawPrompt();
        break;

      case 'error':
        this._clearInputLine();
        this._writeln(`${C.red}  ${msg.text}${C.reset}`);
        this._drawPrompt();
        break;

      case 'pong':
        break;
    }
  }

  // -------------------------------------------------------------------------
  // Terminal input binding
  // -------------------------------------------------------------------------

  _bindTerminal() {
    this.term.onData((data) => {
      // Session picker mode
      if (this.inSessionPicker) {
        this._handlePickerKey(data);
        return;
      }

      for (let i = 0; i < data.length; i++) {
        const ch = data[i];
        const code = ch.charCodeAt(0);

        // ESC sequence (arrow keys etc)
        if (ch === '\x1b' && data[i + 1] === '[') {
          const arrow = data[i + 2];
          if (arrow === 'A') { this._historyUp(); i += 2; continue; }
          if (arrow === 'B') { this._historyDown(); i += 2; continue; }
          if (arrow === 'C') { this._cursorRight(); i += 2; continue; }
          if (arrow === 'D') { this._cursorLeft(); i += 2; continue; }
          i += 2; continue;
        }

        // Enter
        if (ch === '\r' || ch === '\n') {
          this.term.writeln('');
          this._submitInput();
          continue;
        }

        // Backspace
        if (code === 127 || code === 8) {
          this._backspace();
          continue;
        }

        // Ctrl+C
        if (code === 3) {
          if (this.pendingToolCall) {
            this._confirmTool(false, false);
            this._writeln(`${C.gray}  Tool call cancelled.${C.reset}`);
            this._drawPrompt();
          } else {
            this.inputBuf = '';
            this.cursorPos = 0;
            this.term.writeln('^C');
            this._drawPrompt();
          }
          continue;
        }

        // Ctrl+D
        if (code === 4) {
          if (this.inputBuf.length === 0) {
            this._writeln(`${C.gray}  Goodbye.${C.reset}`);
            if (this.ws) this.ws.close();
          }
          continue;
        }

        // Ctrl+U — clear line
        if (code === 21) {
          this.inputBuf = '';
          this.cursorPos = 0;
          this._redrawInput();
          continue;
        }

        // Regular printable character
        if (code >= 32) {
          this.inputBuf = this.inputBuf.slice(0, this.cursorPos) + ch + this.inputBuf.slice(this.cursorPos);
          this.cursorPos++;
          this._redrawInput();
        }
      }
    });
  }

  _handlePickerKey(data) {
    const totalItems = this.pickerSessions.length + 1;

    if (data === '\x1b[A') {
      // Up
      if (this.pickerIdx > 0) {
        this.pickerIdx--;
        this._renderPicker();
      }
    } else if (data === '\x1b[B') {
      // Down
      if (this.pickerIdx < totalItems - 1) {
        this.pickerIdx++;
        this._renderPicker();
      }
    } else if (data === '\r' || data === '\n') {
      this._pickerSelect();
    }
  }

  // -------------------------------------------------------------------------
  // Input line management
  // -------------------------------------------------------------------------

  _drawPrompt() {
    this.inputBuf = '';
    this.cursorPos = 0;
    this.term.write(PROMPT);
  }

  _drawConfirmPrompt() {
    this.inputBuf = '';
    this.cursorPos = 0;
    this.term.write(`${C.bold}${C.yellow}  > ${C.reset}`);
  }

  _redrawInput() {
    // Clear current line content after prompt, rewrite buffer, position cursor
    this.term.write('\x1b[2K\r');
    if (this.pendingToolCall) {
      this.term.write(`${C.bold}${C.yellow}  > ${C.reset}${this.inputBuf}`);
    } else {
      this.term.write(`${PROMPT}${this.inputBuf}`);
    }
    // Move cursor to correct position
    const promptLen = this.pendingToolCall ? 4 : 6; // "  > " or "bear> "
    const targetCol = promptLen + this.cursorPos;
    const endCol = promptLen + this.inputBuf.length;
    if (targetCol < endCol) {
      this.term.write(`\x1b[${endCol - targetCol}D`);
    }
  }

  _clearInputLine() {
    this.term.write('\x1b[2K\r');
  }

  _writeln(text) {
    this.term.writeln(text);
  }

  // -------------------------------------------------------------------------
  // Input editing
  // -------------------------------------------------------------------------

  _backspace() {
    if (this.cursorPos > 0) {
      this.inputBuf = this.inputBuf.slice(0, this.cursorPos - 1) + this.inputBuf.slice(this.cursorPos);
      this.cursorPos--;
      this._redrawInput();
    }
  }

  _cursorLeft() {
    if (this.cursorPos > 0) {
      this.cursorPos--;
      this.term.write('\x1b[D');
    }
  }

  _cursorRight() {
    if (this.cursorPos < this.inputBuf.length) {
      this.cursorPos++;
      this.term.write('\x1b[C');
    }
  }

  _historyUp() {
    if (this.history.length === 0) return;
    if (this.historyIdx === -1) {
      this.savedInput = this.inputBuf;
      this.historyIdx = this.history.length - 1;
    } else if (this.historyIdx > 0) {
      this.historyIdx--;
    }
    this.inputBuf = this.history[this.historyIdx];
    this.cursorPos = this.inputBuf.length;
    this._redrawInput();
  }

  _historyDown() {
    if (this.historyIdx === -1) return;
    if (this.historyIdx < this.history.length - 1) {
      this.historyIdx++;
      this.inputBuf = this.history[this.historyIdx];
    } else {
      this.historyIdx = -1;
      this.inputBuf = this.savedInput;
    }
    this.cursorPos = this.inputBuf.length;
    this._redrawInput();
  }

  // -------------------------------------------------------------------------
  // Submit input
  // -------------------------------------------------------------------------

  _submitInput() {
    const text = this.inputBuf.trim();
    this.inputBuf = '';
    this.cursorPos = 0;

    if (!text) {
      this._drawPrompt();
      return;
    }

    // Push to history
    if (this.history.length === 0 || this.history[this.history.length - 1] !== text) {
      this.history.push(text);
    }
    this.historyIdx = -1;
    this.savedInput = '';

    // Tool confirmation mode
    if (this.pendingToolCall) {
      const lower = text.toLowerCase();
      if (lower === 'y' || lower === 'yes') {
        this._confirmTool(true, false);
      } else if (lower === 'n' || lower === 'no') {
        this._confirmTool(false, false);
      } else if (lower === 'a' || lower === 'always') {
        this._confirmTool(true, true);
      } else {
        this._writeln(`${C.gray}  Please type y, n, or a${C.reset}`);
        this._drawConfirmPrompt();
      }
      return;
    }

    // Slash commands
    if (text === '/help') {
      this._showHelp();
      this._drawPrompt();
      return;
    }

    if (text === '/allowed') {
      if (this.autoApproved.size === 0) {
        this._writeln(`${C.gray}  No auto-approved commands.${C.reset}`);
      } else {
        this._writeln(`${C.white}  Auto-approved: ${[...this.autoApproved].sort().join(', ')}${C.reset}`);
      }
      this._drawPrompt();
      return;
    }

    if (text === '/end') {
      this._writeln(`${C.gray}  Ending session…${C.reset}`);
      if (this.ws) { this.ws.close(); this.ws = null; }
      this.term.writeln('');
      this._showSessionPicker();
      return;
    }

    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      this._writeln(`${C.red}  Not connected. Use /end to pick a session.${C.reset}`);
      this._drawPrompt();
      return;
    }

    if (text === '/ps') {
      this._sendJson({ type: 'process_list' });
      return;
    }

    const killMatch = text.match(/^\/kill\s+(\d+)$/);
    if (killMatch) {
      this._sendJson({ type: 'process_kill', pid: parseInt(killMatch[1]) });
      return;
    }

    const sendMatch = text.match(/^\/send\s+(\d+)\s+(.+)$/);
    if (sendMatch) {
      this._sendJson({ type: 'process_input', pid: parseInt(sendMatch[1]), text: sendMatch[2] });
      return;
    }

    // Regular chat input
    this._sendJson({ type: 'input', text: text });
    // Don't draw prompt yet — wait for server response
  }

  // -------------------------------------------------------------------------
  // Tool confirmation
  // -------------------------------------------------------------------------

  _confirmTool(approved, alwaysAllow) {
    if (!this.pendingToolCall) return;
    const tc = this.pendingToolCall;
    const baseCmd = this._extractBaseCommand(tc);

    if (alwaysAllow) {
      this.autoApproved.add(baseCmd);
      this._writeln(`${C.yellow}  '${baseCmd}' will be auto-approved for this session.${C.reset}`);
    }

    const verdict = approved
      ? `${C.green}  ✓ Approved${C.reset}`
      : `${C.red}  ✗ Denied${C.reset}`;
    this._writeln(verdict);

    this._sendJson({ type: 'tool_confirm', tool_call_id: tc.id, approved });
    this.pendingToolCall = null;
    // Don't draw prompt — wait for tool_output or next message
  }

  _extractBaseCommand(toolCall) {
    if (toolCall.name === 'run_command') {
      const cmd = toolCall.arguments?.command || '';
      const tokens = cmd.split(/\s+/);
      for (const token of tokens) {
        if (token.includes('=') && !token.startsWith('-')) continue;
        if (token === 'sudo' || token === 'env') continue;
        const base = token.split('/').pop();
        return base || token;
      }
      return cmd;
    }
    return toolCall.name;
  }

  // -------------------------------------------------------------------------
  // Help
  // -------------------------------------------------------------------------

  _showHelp() {
    const lines = [
      '',
      `${C.bold}${C.white}  Commands:${C.reset}`,
      `${C.gray}    /ps              ${C.white}List background processes${C.reset}`,
      `${C.gray}    /kill <pid>      ${C.white}Kill a background process${C.reset}`,
      `${C.gray}    /send <pid> <t>  ${C.white}Send stdin to a process${C.reset}`,
      `${C.gray}    /allowed         ${C.white}Show auto-approved commands${C.reset}`,
      `${C.gray}    /end             ${C.white}End session, pick another${C.reset}`,
      `${C.gray}    /help            ${C.white}Show this help${C.reset}`,
      '',
      `${C.bold}${C.white}  Tool confirmations:${C.reset}`,
      `${C.green}    y/yes            ${C.white}Approve this tool call${C.reset}`,
      `${C.red}    n/no             ${C.white}Deny this tool call${C.reset}`,
      `${C.yellow}    a/always         ${C.white}Approve & auto-approve for session${C.reset}`,
      '',
    ];
    for (const l of lines) this._writeln(l);
  }

  // -------------------------------------------------------------------------
  // WebSocket helpers
  // -------------------------------------------------------------------------

  _sendJson(obj) {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(obj));
    }
  }
}
