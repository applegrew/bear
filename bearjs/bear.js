// ---------------------------------------------------------------------------
// Bear Browser Client — xterm.js powered terminal connecting to bear-server
// ---------------------------------------------------------------------------

const DEFAULT_HOST = '127.0.0.1:49321';
const SERVER_URL = `http://${DEFAULT_HOST}`;
const WS_BASE   = `ws://${DEFAULT_HOST}`;

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
const PROMPT_CMD = `${C.bold}${C.yellow}cmd-> ${C.reset}`;

const SLASH_COMMANDS = [
  { cmd: '/ps', desc: 'List background processes' },
  { cmd: '/kill', desc: 'Kill a background process' },
  { cmd: '/send', desc: 'Send stdin to a process' },
  { cmd: '/session name', desc: 'Name the current session' },
  { cmd: '/allowed', desc: 'Show auto-approved commands' },
  { cmd: '/exit', desc: 'Disconnect, keep session alive' },
  { cmd: '/end', desc: 'End session, pick another' },
  { cmd: '/help', desc: 'Show help' },
];

function matchingSlashCommands(input) {
  if (!input.startsWith('/')) return [];
  const typed = input.split(/\s/)[0] || input;
  return SLASH_COMMANDS
    .filter(s => s.cmd.startsWith(typed) || typed.startsWith(s.cmd))
    .slice(0, 3);
}

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

    // Streaming state
    this._streaming = false;

    // Slash command dropdown state
    this._dropdownLines = 0;
    this._dropdownIdx = -1; // -1 = no selection

    // Last tool tracking for tool-specific output rendering
    this._lastToolName = '';
    this._lastToolArgs = {};

    // Tool confirmation state
    this.pendingToolCall = null;
    this.autoApproved = new Set();

    // Session picker state
    this.inSessionPicker = false;
    this.pickerSessions = [];
    this.pickerIdx = 0;
    this.pickerRendered = false;

    // User prompt state
    this.inUserPrompt = false;
    this.userPromptId = null;
    this.userPromptOptions = [];
    this.userPromptMulti = false;
    this.userPromptIdx = 0;
    this.userPromptSelected = [];
    this.userPromptRendered = false;

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
        label: s.name || s.id.substring(0, 8) + '…',
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
        if (!this._streaming) {
          this._clearInputLine();
          this._streaming = true;
          this.term.write(`${C.green}  `);
        }
        // Write chunk inline, indenting any newlines
        this.term.write(`${C.green}${msg.text.replace(/\n/g, '\r\n  ')}${C.reset}`);
        break;

      case 'assistant_text_done':
        if (this._streaming) {
          this._streaming = false;
          this.term.write(`${C.reset}\r\n`);
        }
        this._restorePrompt();
        break;

      case 'tool_request': {
        const tc = msg.tool_call;
        this._lastToolName = tc.name;
        this._lastToolArgs = tc.arguments;
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
        this._renderToolOutput(this._lastToolName || '', this._lastToolArgs || {}, msg.output);
        this._restorePrompt();
        break;

      case 'process_started':
        this._clearInputLine();
        this._writeln(`${C.magenta}  [proc] Started pid=${msg.info.pid} cmd=${msg.info.command}${C.reset}`);
        this._restorePrompt();
        break;

      case 'process_output':
        this._clearInputLine();
        this._writeln(`${C.magenta}  [${msg.pid}] ${msg.text}${C.reset}`);
        this._restorePrompt();
        break;

      case 'process_exited': {
        const code = msg.code !== null && msg.code !== undefined ? msg.code : 'unknown';
        this._clearInputLine();
        this._writeln(`${C.magenta}  [proc] Process ${msg.pid} exited (code ${code})${C.reset}`);
        this._restorePrompt();
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
        this._restorePrompt();
        break;

      case 'session_renamed':
        this._clearInputLine();
        this._writeln(`${C.green}  Session renamed to: ${msg.name}${C.reset}`);
        this._restorePrompt();
        break;

      case 'notice':
        this._clearInputLine();
        this._writeln(`${C.gray}  ${msg.text}${C.reset}`);
        this._restorePrompt();
        break;

      case 'error':
        this._clearInputLine();
        this._writeln(`${C.red}  ${msg.text}${C.reset}`);
        this._restorePrompt();
        break;

      case 'thinking':
        this._clearInputLine();
        this._writeln(`${C.dim}${C.gray}  ⟳ Thinking…${C.reset}`);
        break;

      case 'user_prompt':
        this._clearInputLine();
        this.inUserPrompt = true;
        this.userPromptId = msg.prompt_id;
        this.userPromptOptions = msg.options;
        this.userPromptMulti = msg.multi;
        this.userPromptIdx = 0;
        this.userPromptSelected = new Array(msg.options.length).fill(false);
        this.userPromptRendered = false;
        this._writeln(`${C.bold}${C.cyan}  ${msg.question}${C.reset}`);
        this._writeln('');
        this._renderUserPrompt();
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

      // User prompt mode
      if (this.inUserPrompt) {
        this._handleUserPromptKey(data);
        return;
      }

      for (let i = 0; i < data.length; i++) {
        const ch = data[i];
        const code = ch.charCodeAt(0);

        // ESC sequence (arrow keys etc)
        if (ch === '\x1b' && data[i + 1] === '[') {
          const arrow = data[i + 2];
          i += 2;
          if (this._dropdownActive()) {
            if (arrow === 'A') { this._dropdownUp(); this._redrawInput(); continue; }
            if (arrow === 'B') { this._dropdownDown(); this._redrawInput(); continue; }
            // Left/Right: reset selection, fall through to normal behavior
            this._dropdownIdx = -1;
          }
          if (arrow === 'A') { this._historyUp(); continue; }
          if (arrow === 'B') { this._historyDown(); continue; }
          if (arrow === 'C') { this._cursorRight(); continue; }
          if (arrow === 'D') { this._cursorLeft(); continue; }
          continue;
        }

        // Bare Esc (not part of arrow sequence)
        if (code === 27) {
          if (this._dropdownActive()) {
            this.inputBuf = '';
            this.cursorPos = 0;
            this._dropdownIdx = -1;
            this._redrawInput();
          }
          continue;
        }

        // Tab
        if (code === 9) {
          if (this._dropdownActive()) {
            this._acceptDropdown();
            this._redrawInput();
          }
          continue;
        }

        // Enter
        if (ch === '\r' || ch === '\n') {
          if (this._dropdownActive() && this._dropdownIdx >= 0) {
            this._acceptDropdown();
            this._redrawInput();
            continue;
          }
          this._clearDropdown();
          this.term.writeln('');
          this._submitInput();
          continue;
        }

        // Backspace
        if (code === 127 || code === 8) {
          this._dropdownIdx = -1;
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

        // Ctrl+D — disabled in browser client
        if (code === 4) {
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
          this._dropdownIdx = -1;
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
  // User prompt (interactive option selection)
  // -------------------------------------------------------------------------

  _handleUserPromptKey(data) {
    const total = this.userPromptOptions.length;

    if (data === '\x1b[A') {
      // Up
      if (this.userPromptIdx > 0) {
        this.userPromptIdx--;
        this._renderUserPrompt();
      }
    } else if (data === '\x1b[B') {
      // Down
      if (this.userPromptIdx < total - 1) {
        this.userPromptIdx++;
        this._renderUserPrompt();
      }
    } else if (data === ' ' && this.userPromptMulti) {
      // Toggle selection in multi mode
      this.userPromptSelected[this.userPromptIdx] = !this.userPromptSelected[this.userPromptIdx];
      this._renderUserPrompt();
    } else if (data === '\r' || data === '\n') {
      this._userPromptSelect();
    }
  }

  _renderUserPrompt() {
    const opts = this.userPromptOptions;
    const totalLines = opts.length + 1; // options + hint

    // Clear previous render
    if (this.userPromptRendered) {
      for (let i = 0; i < totalLines; i++) {
        this.term.write('\x1b[A\x1b[2K');
      }
    }
    this.userPromptRendered = true;

    for (let i = 0; i < opts.length; i++) {
      const focused = i === this.userPromptIdx;
      if (this.userPromptMulti) {
        const check = this.userPromptSelected[i] ? '[x]' : '[ ]';
        if (focused) {
          this.term.writeln(`${C.bold}${C.yellow}  ${check} ${C.white}${opts[i]}${C.reset}`);
        } else {
          this.term.writeln(`${C.gray}  ${check} ${opts[i]}${C.reset}`);
        }
      } else {
        if (focused) {
          this.term.writeln(`${C.bold}${C.blue}  ❯ ${C.white}${opts[i]}${C.reset}`);
        } else {
          this.term.writeln(`${C.gray}    ${opts[i]}${C.reset}`);
        }
      }
    }

    const hint = this.userPromptMulti
      ? `${C.gray}  ↑/↓ navigate, Space toggle, Enter confirm${C.reset}`
      : `${C.gray}  ↑/↓ navigate, Enter select${C.reset}`;
    this.term.writeln(hint);
  }

  _userPromptSelect() {
    this.inUserPrompt = false;
    this.term.writeln('');

    let selected;
    if (this.userPromptMulti) {
      selected = [];
      for (let i = 0; i < this.userPromptSelected.length; i++) {
        if (this.userPromptSelected[i]) selected.push(i);
      }
    } else {
      selected = [this.userPromptIdx];
    }

    this._sendJson({
      type: 'user_prompt_response',
      prompt_id: this.userPromptId,
      selected,
    });
    this._drawPrompt();
  }

  // -------------------------------------------------------------------------
  // Input line management
  // -------------------------------------------------------------------------

  _drawPrompt() {
    this.inputBuf = '';
    this.cursorPos = 0;
    this._dropdownIdx = -1;
    this._clearDropdown();
    this.term.write(PROMPT);
  }

  _restorePrompt() {
    // Redraw prompt preserving any in-progress user input
    this._clearDropdown();
    if (this.pendingToolCall) {
      this.term.write(`${C.bold}${C.yellow}  > ${C.reset}${this.inputBuf}`);
    } else {
      const p = this.inputBuf.startsWith('/') ? PROMPT_CMD : PROMPT;
      this.term.write(`${p}${this.inputBuf}`);
    }
    // Reposition cursor if not at end
    const back = this.inputBuf.length - this.cursorPos;
    if (back > 0) {
      this.term.write(`\x1b[${back}D`);
    }
    this._renderDropdown();
  }

  _drawConfirmPrompt() {
    this.inputBuf = '';
    this.cursorPos = 0;
    this.term.write(`${C.bold}${C.yellow}  > ${C.reset}`);
  }

  _redrawInput() {
    // Clear dropdown first, then redraw prompt line
    this._clearDropdown();
    this.term.write('\x1b[2K\r');
    if (this.pendingToolCall) {
      this.term.write(`${C.bold}${C.yellow}  > ${C.reset}${this.inputBuf}`);
    } else {
      const p = this.inputBuf.startsWith('/') ? PROMPT_CMD : PROMPT;
      this.term.write(`${p}${this.inputBuf}`);
    }
    // Move cursor to correct position
    const promptLen = this.pendingToolCall ? 4 : 6; // "  > " or "bear> " / "cmd-> "
    const targetCol = promptLen + this.cursorPos;
    const endCol = promptLen + this.inputBuf.length;
    if (targetCol < endCol) {
      this.term.write(`\x1b[${endCol - targetCol}D`);
    }
    // Show dropdown if in slash mode
    this._renderDropdown();
  }

  _clearInputLine() {
    this._clearDropdown();
    this.term.write('\x1b[2K\r');
  }

  _clearDropdown() {
    if (this._dropdownLines > 0) {
      this.term.write('\x1b[s');
      for (let i = 0; i < this._dropdownLines; i++) {
        this.term.write('\r\n\x1b[2K');
      }
      this.term.write('\x1b[u');
      this._dropdownLines = 0;
    }
  }

  _renderDropdown() {
    if (this.pendingToolCall) return;
    const matches = matchingSlashCommands(this.inputBuf);
    if (matches.length === 0) {
      this._dropdownIdx = -1;
      return;
    }
    // Clamp index
    if (this._dropdownIdx >= matches.length) {
      this._dropdownIdx = matches.length - 1;
    }
    this.term.write('\x1b[s');
    for (let i = 0; i < matches.length; i++) {
      const { cmd, desc } = matches[i];
      const selected = i === this._dropdownIdx;
      if (selected) {
        this.term.write(`\r\n\x1b[2K${C.yellow}    ❯ ${C.white}${cmd}${C.gray}  ${desc}${C.reset}`);
      } else {
        this.term.write(`\r\n\x1b[2K${C.gray}      ${C.yellow}${cmd}${C.gray}  ${desc}${C.reset}`);
      }
    }
    this._dropdownLines = matches.length;
    this.term.write('\x1b[u');
  }

  _dropdownActive() {
    return this._dropdownLines > 0 && this.inputBuf.startsWith('/');
  }

  _dropdownUp() {
    const matches = matchingSlashCommands(this.inputBuf);
    if (matches.length === 0) return;
    if (this._dropdownIdx <= 0) {
      this._dropdownIdx = matches.length - 1;
    } else {
      this._dropdownIdx--;
    }
  }

  _dropdownDown() {
    const matches = matchingSlashCommands(this.inputBuf);
    if (matches.length === 0) return;
    if (this._dropdownIdx < 0 || this._dropdownIdx >= matches.length - 1) {
      this._dropdownIdx = 0;
    } else {
      this._dropdownIdx++;
    }
  }

  _acceptDropdown() {
    const matches = matchingSlashCommands(this.inputBuf);
    const idx = this._dropdownIdx >= 0 ? this._dropdownIdx : 0;
    if (idx < matches.length) {
      this.inputBuf = matches[idx].cmd + ' ';
      this.cursorPos = this.inputBuf.length;
    }
    this._dropdownIdx = -1;
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
    this._clearDropdown();
    this._dropdownIdx = -1;
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
      if (this.ws && this.ws.readyState === WebSocket.OPEN) {
        this._sendJson({ type: 'session_end' });
      }
      this._writeln(`${C.gray}  Session ended.${C.reset}`);
      if (this.ws) { this.ws.close(); this.ws = null; }
      this.term.writeln('');
      this._showSessionPicker();
      return;
    }

    if (text === '/exit') {
      this._writeln(`${C.gray}  Disconnecting. Session preserved.${C.reset}`);
      if (this.ws) { this.ws.close(); this.ws = null; }
      this.term.writeln('');
      this._showSessionPicker();
      return;
    }

    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      this._writeln(`${C.red}  Not connected. Use /exit to pick a session.${C.reset}`);
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

    const sessionMatch = text.match(/^\/session\s+name\s+(.+)$/);
    if (sessionMatch) {
      const name = sessionMatch[1].trim();
      if (!name) {
        this._writeln(`${C.red}  Usage: /session name <session name>${C.reset}`);
        this._drawPrompt();
      } else {
        this._sendJson({ type: 'session_rename', name });
      }
      return;
    }
    if (text.startsWith('/session')) {
      this._writeln(`${C.red}  Usage: /session name <session name>${C.reset}`);
      this._drawPrompt();
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
  // Tool-specific output rendering
  // -------------------------------------------------------------------------

  _renderToolOutput(toolName, toolArgs, output) {
    const MAX_LINES = 20;

    switch (toolName) {
      case 'read_file': {
        const path = toolArgs.path || '?';
        if (output.startsWith('Error')) {
          this._writeln(`${C.red}  ✗ ${output}${C.reset}`);
        } else {
          const lineCount = output.split('\n').length;
          this._writeln(`${C.green}  ✓ Read ${path} (${lineCount} lines)${C.reset}`);
        }
        break;
      }
      case 'write_file': {
        const color = output.startsWith('Error') ? C.red : C.green;
        const icon = output.startsWith('Error') ? '✗' : '✓';
        this._writeln(`${color}  ${icon} ${output}${C.reset}`);
        break;
      }
      case 'edit_file':
      case 'patch_file': {
        const isErr = output.startsWith('Error') || output.startsWith('Patch failed');
        const color = isErr ? C.red : C.green;
        const icon = isErr ? '✗' : '✓';
        this._writeln(`${color}  ${icon} ${output}${C.reset}`);
        break;
      }
      case 'run_command':
        this._writeToolTruncated(output, MAX_LINES);
        break;
      case 'list_files': {
        const count = output.split('\n').filter(l => l.length > 0).length;
        this._writeln(`${C.green}  ✓ ${count} entries${C.reset}`);
        this._writeToolTruncated(output, MAX_LINES);
        break;
      }
      case 'search_text': {
        if (output === 'No matches found.') {
          this._writeln(`${C.gray}  │ ${output}${C.reset}`);
        } else {
          const count = output.split('\n').filter(l => l.length > 0 && !l.startsWith('[')).length;
          this._writeln(`${C.green}  ✓ ${count} matches${C.reset}`);
          this._writeToolTruncated(output, MAX_LINES);
        }
        break;
      }
      case 'undo': {
        const isNoop = output.startsWith('Error') || output === 'Nothing to undo.';
        const color = isNoop ? C.gray : C.green;
        const icon = isNoop ? '│' : '✓';
        this._writeln(`${color}  ${icon} ${output}${C.reset}`);
        break;
      }
      case 'user_prompt_options':
        this._writeln(`${C.cyan}  │ ${output}${C.reset}`);
        break;
      default:
        this._writeToolTruncated(output, MAX_LINES);
        break;
    }
  }

  _writeToolTruncated(output, maxLines) {
    const lines = output.split('\n');
    const total = lines.length;
    if (total <= maxLines) {
      for (const line of lines) {
        this._writeln(`${C.gray}  │ ${line}${C.reset}`);
      }
      return;
    }
    const head = Math.floor(maxLines / 2);
    const tail = maxLines - head;
    for (let i = 0; i < head; i++) {
      this._writeln(`${C.gray}  │ ${lines[i]}${C.reset}`);
    }
    this._writeln(`${C.dim}${C.gray}  │   … (${total - head - tail} lines hidden) …${C.reset}`);
    for (let i = total - tail; i < total; i++) {
      this._writeln(`${C.gray}  │ ${lines[i]}${C.reset}`);
    }
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
      `${C.gray}    /session name <n>${C.white} Name the current session${C.reset}`,
      `${C.gray}    /allowed         ${C.white}Show auto-approved commands${C.reset}`,
      `${C.gray}    /exit            ${C.white}Disconnect, keep session alive${C.reset}`,
      `${C.gray}    /end             ${C.white}End session, pick another${C.reset}`,
      `${C.gray}    /help            ${C.white}Show this help${C.reset}`,
      '',
      `${C.bold}${C.white}  Tool confirmations:${C.reset}`,
      `${C.green}    y/yes            ${C.white}Approve this tool call${C.reset}`,
      `${C.red}    n/no             ${C.white}Deny this tool call${C.reset}`,
      `${C.yellow}    a/always         ${C.white}Approve & auto-approve for session${C.reset}`,
      '',
      `${C.bold}${C.white}  Agent tools:${C.reset}`,
      `${C.cyan}    run_command      ${C.white}Execute shell commands${C.reset}`,
      `${C.cyan}    read_file        ${C.white}Read file contents${C.reset}`,
      `${C.cyan}    write_file       ${C.white}Create/overwrite files${C.reset}`,
      `${C.cyan}    edit_file        ${C.white}Surgical find-and-replace${C.reset}`,
      `${C.cyan}    patch_file       ${C.white}Apply unified diffs${C.reset}`,
      `${C.cyan}    list_files       ${C.white}Directory listing with glob${C.reset}`,
      `${C.cyan}    search_text      ${C.white}Regex search across files${C.reset}`,
      `${C.cyan}    undo             ${C.white}Revert file changes${C.reset}`,
      `${C.cyan}    user_prompt_options ${C.white}Present choices to user${C.reset}`,
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
