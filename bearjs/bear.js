// ---------------------------------------------------------------------------
// Bear Browser Client — OpenCode-style TUI powered by xterm.js
// ---------------------------------------------------------------------------
const BEAR_VERSION = '0.2.2.2';
// Relay configuration: these globals must be set by the hosting page.
// bear.js communicates exclusively via the public server, which proxies
// all signaling (offer, answer, ICE) to the relay on behalf of the browser.
const RELAY_ROOM = (typeof window !== 'undefined' && window.BEAR_ROOM_ID) ? window.BEAR_ROOM_ID : null;
const PUBLIC_URL = (typeof window !== 'undefined' && window.BEAR_PUBLIC_URL != null) ? window.BEAR_PUBLIC_URL : '';
const HOME_URL = (typeof window !== 'undefined' && window.BEAR_HOME) ? window.BEAR_HOME : '/dashboard';

// ICE servers: STUN defaults. TURN servers are fetched dynamically before each
// connection via GET /api/signal/turn-credentials. The legacy BEAR_ICE_SERVERS
// global is used as a fallback if the dynamic fetch fails.
const ICE_STUN = [
  { urls: 'stun:stun.l.google.com:19302' },
  { urls: 'stun:stun1.l.google.com:19302' },
];
const ICE_FALLBACK_TURN = (typeof window !== 'undefined' && Array.isArray(window.BEAR_ICE_SERVERS))
  ? window.BEAR_ICE_SERVERS : [];

// ANSI color helpers (Tokyo Night palette)
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
  bgGray:  '\x1b[48;5;236m',
};

// Tool confirmation picker
const TOOL_CONFIRM_LABELS = ['Approve', 'Deny', 'Always approve for session'];

// Spinner frames
const SPINNER = ['·····', '●····', '·●···', '··●··', '···●·', '····●', '·····'];


// ---------------------------------------------------------------------------
// Markdown → ANSI rendering
// ---------------------------------------------------------------------------

const MD = {
  reset:      '\x1b[0m',
  bold:       '\x1b[1m',
  italic:     '\x1b[3m',
  h1:         '\x1b[1m\x1b[38;5;80m',       // bold cyan
  h2:         '\x1b[1m\x1b[38;5;114m',      // bold green
  h3:         '\x1b[1m\x1b[38;5;180m',      // bold yellow
  codeInline: '\x1b[38;5;222m',             // warm yellow
  codeBlock:  '\x1b[38;5;246m',             // light gray
  codeLang:   '\x1b[38;5;102m',             // dim gray
  bullet:     '\x1b[38;5;80m',              // cyan
  hrule:      '\x1b[38;5;240m',             // dim
  link:       '\x1b[4m\x1b[38;5;75m',       // underline blue
  green:      '\x1b[38;5;114m',             // default text
};

/**
 * Render inline markdown: **bold**, *italic*, `code`, [links](url)
 */
function renderMdInline(text) {
  let out = '';
  const chars = [...text];
  const len = chars.length;
  let i = 0;

  while (i < len) {
    // Bold: **text**
    if (i + 1 < len && chars[i] === '*' && chars[i + 1] === '*') {
      const end = findClosingDouble(chars, i + 2, '*', '*');
      if (end !== -1) {
        const inner = chars.slice(i + 2, end).join('');
        out += `${MD.bold}${inner}${MD.reset}${MD.green}`;
        i = end + 2;
        continue;
      }
    }

    // Italic: *text*
    if (chars[i] === '*' && (i + 1 >= len || chars[i + 1] !== '*')) {
      const end = findClosingSingle(chars, i + 1, '*');
      if (end !== -1) {
        const inner = chars.slice(i + 1, end).join('');
        out += `${MD.italic}${inner}${MD.reset}${MD.green}`;
        i = end + 1;
        continue;
      }
    }

    // Inline code: `code`
    if (chars[i] === '`') {
      const end = findClosingSingle(chars, i + 1, '`');
      if (end !== -1) {
        const inner = chars.slice(i + 1, end).join('');
        out += `${MD.codeInline}${inner}${MD.reset}${MD.green}`;
        i = end + 1;
        continue;
      }
    }

    // Link: [text](url)
    if (chars[i] === '[') {
      const closeBracket = findClosingSingle(chars, i + 1, ']');
      if (closeBracket !== -1 && closeBracket + 1 < len && chars[closeBracket + 1] === '(') {
        const closeParen = findClosingSingle(chars, closeBracket + 2, ')');
        if (closeParen !== -1) {
          const linkText = chars.slice(i + 1, closeBracket).join('');
          out += `${MD.link}${linkText}${MD.reset}${MD.green}`;
          i = closeParen + 1;
          continue;
        }
      }
    }

    out += chars[i];
    i++;
  }
  return out;
}

function findClosingDouble(chars, start, c1, c2) {
  for (let j = start; j + 1 < chars.length; j++) {
    if (chars[j] === c1 && chars[j + 1] === c2) return j;
  }
  return -1;
}

function findClosingSingle(chars, start, delim) {
  for (let j = start; j < chars.length; j++) {
    if (chars[j] === delim) return j;
  }
  return -1;
}

/**
 * Render a single line of markdown to ANSI. `state` tracks code-block across lines.
 * state = { inCodeBlock: false }
 * Returns an array of rendered strings (usually 1 element).
 */
function renderMdLine(line, state) {
  const trimmed = line.trim();

  // Code block fences
  if (trimmed.startsWith('```')) {
    if (state.inCodeBlock) {
      state.inCodeBlock = false;
      return [`${MD.codeBlock}\`\`\`${MD.reset}`];
    } else {
      state.inCodeBlock = true;
      const lang = trimmed.replace(/^`+/, '').trim();
      if (lang) {
        return [`${MD.codeBlock}\`\`\`${MD.codeLang}${lang}${MD.reset}`];
      }
      return [`${MD.codeBlock}\`\`\`${MD.reset}`];
    }
  }

  // Inside code block
  if (state.inCodeBlock) {
    return [`${MD.codeBlock}${line}${MD.reset}`];
  }

  // Horizontal rule
  if (/^[-*_]{3,}$/.test(trimmed.replace(/ /g, ''))) {
    if (trimmed.length >= 3) {
      return [`${MD.hrule}─────────────────────────────${MD.reset}`];
    }
  }

  // Headers
  if (trimmed.startsWith('### ')) {
    return [`${MD.h3}${trimmed.slice(4)}${MD.reset}`];
  }
  if (trimmed.startsWith('## ')) {
    return [`${MD.h2}${trimmed.slice(3)}${MD.reset}`];
  }
  if (trimmed.startsWith('# ')) {
    return [`${MD.h1}${trimmed.slice(2)}${MD.reset}`];
  }

  // Unordered list
  const ulMatch = trimmed.match(/^[-*] (.*)$/);
  if (ulMatch) {
    return [`${MD.bullet}  • ${MD.green}${renderMdInline(ulMatch[1])}${MD.reset}`];
  }

  // Numbered list
  const olMatch = trimmed.match(/^(\d+)\. (.*)$/);
  if (olMatch) {
    return [`${MD.bullet}  ${olMatch[1]}. ${MD.green}${renderMdInline(olMatch[2])}${MD.reset}`];
  }

  // Empty line
  if (!trimmed) return [''];

  // Regular line with inline formatting
  return [`${MD.green}${renderMdInline(line)}${MD.reset}`];
}

// ---------------------------------------------------------------------------
// ANSI-aware text wrapping
// ---------------------------------------------------------------------------

/**
 * Compute visible length of a string, stripping ANSI escape sequences.
 */
function visibleLen(s) {
  let len = 0;
  let inEsc = false;
  for (const c of s) {
    if (inEsc) {
      if (/[a-zA-Z]/.test(c)) inEsc = false;
    } else if (c === '\x1b') {
      inEsc = true;
    } else {
      len++;
    }
  }
  return len;
}

/**
 * Wrap a line into multiple visual rows of at most `max` visible characters,
 * preserving ANSI escape codes across wraps.
 */
function _ansiCategory(seq) {
  const params = seq.slice(2, -1); // strip \x1b[ and trailing letter
  if (params.startsWith('38;') || params.startsWith('38 ')) return 'fg';
  if (params.startsWith('48;') || params.startsWith('48 ')) return 'bg';
  return params; // e.g. '1' for bold, '3' for italic
}

function wrapVisible(s, max) {
  if (max <= 0) return [s];
  const rows = [];
  let current = '';
  let vis = 0;
  let inEsc = false;
  let activeAnsi = new Map(); // category -> sequence (replaces duplicates)

  for (const c of s) {
    if (inEsc) {
      current += c;
      if (/[a-zA-Z]/.test(c)) {
        inEsc = false;
        // Extract the escape sequence we just finished
        const escStart = current.lastIndexOf('\x1b');
        if (escStart !== -1) {
          const seq = current.slice(escStart);
          if (seq === '\x1b[0m' || seq === '\x1b[m') {
            activeAnsi = new Map();
          } else {
            activeAnsi.set(_ansiCategory(seq), seq);
          }
        }
      }
    } else if (c === '\x1b') {
      inEsc = true;
      current += c;
    } else {
      if (vis >= max) {
        current += '\x1b[0m';
        rows.push(current);
        current = [...activeAnsi.values()].join('');
        vis = 0;
      }
      current += c;
      vis++;
    }
  }
  if (current || rows.length === 0) {
    rows.push(current);
  }
  return rows;
}

function matchingSlashCommands(input, commands) {
  if (!input.startsWith('/')) return [];
  const typed = input.trimEnd();
  return commands
    .filter(s => s.cmd.startsWith(typed) || typed.startsWith(s.cmd))
    .slice(0, 5);
}

/** Format a tool call into human-readable description lines for the card UI. */
function formatToolDescription(name, args) {
  switch (name) {
    case 'run_command':
      return [`$ ${args.command || '(unknown)'}`];
    case 'read_file':
      return [`Reading: ${args.path || '(unknown)'}`];
    case 'write_file':
      return [`Writing: ${args.path || '(unknown)'}`];
    case 'edit_file': {
      const find = (args.find || '').substring(0, 60);
      return [`Editing: ${args.path || '(unknown)'}`, `Find: ${find}…`];
    }
    case 'patch_file':
      return [`Patching: ${args.path || '(unknown)'}`];
    case 'list_files':
      return [`Listing: ${args.path || '.'}  (glob: ${args.glob || '*'})`];
    case 'search_text':
      return [`Searching: "${args.pattern || '(unknown)'}" in ${args.path || '.'}`];
    case 'undo':
      return [`Undo ${args.steps || 1} step(s)`];
    default: {
      if (args && typeof args === 'object') {
        return Object.entries(args).map(([k, v]) => {
          const s = typeof v === 'string' ? v : JSON.stringify(v);
          return `${k}: ${s.length > 60 ? s.substring(0, 60) + '…' : s}`;
        });
      }
      return [JSON.stringify(args)];
    }
  }
}

/** One-line summary of a tool call for the approval picker. */
function toolSummary(name, args) {
  switch (name) {
    case 'run_command': return `$ ${args.command || '(unknown)'}`;
    case 'read_file':   return `read ${args.path || '?'}`;
    case 'write_file':  return `write ${args.path || '?'}`;
    case 'edit_file':   return `edit ${args.path || '?'}`;
    case 'patch_file':  return `patch ${args.path || '?'}`;
    case 'list_files':  return `ls ${args.path || '.'}`;
    case 'search_text': return `grep "${args.pattern || '?'}"`;
    default:            return name;
  }
}

// ---------------------------------------------------------------------------
// BearClient class
// ---------------------------------------------------------------------------

export class BearClient {
  constructor(term, fitAddon) {
    this.term = term;
    this.fitAddon = fitAddon;
    this.ws = null;
    this.pc = null;
    this.dc = null;
    this._connId = null;
    this._icePollTimer = null;
    this.sessionId = null;
    this._audioCtx = null;

    // DOM elements
    this._inputField = document.getElementById('input-field');
    this._inputPrompt = document.getElementById('input-prompt');
    this._sendBtn = document.getElementById('send-btn');
    this._pickerOverlay = document.getElementById('picker-overlay');
    this._slashDropdown = document.getElementById('slash-dropdown');
    this._statusBar = document.getElementById('status-bar');
    this._statusLeft = this._statusBar.querySelector('.status-left');
    this._statusSpinner = this._statusBar.querySelector('.spinner');
    this._statusSession = this._statusBar.querySelector('.session-name');
    this._statusRight = this._statusBar.querySelector('.status-right');

    // Input state — history only; text lives in DOM input
    this.history = [];
    this.historyIdx = -1;
    this.savedInput = '';

    // Streaming state
    this._streaming = false;
    this._streamBuf = '';
    this._thinkingLineShown = false;

    // Spinner
    this._spinnerFrame = 0;
    this._spinnerTimer = null;

    // Session info
    this._sessionName = '';
    this._sessionCwd = '';

    // Output buffer (array of ANSI-colored strings)
    this._outputLines = [];
    this._scrollOffset = 0; // 0 = bottom

    // Slash command state
    this._dropdownIdx = -1;
    this.slashCommands = [];

    // Echo suppression: skip the next UserInput echo from the server
    this._awaitingInputEcho = false;

    // Heartbeat: detect stale connections
    this._heartbeatTimer = null;
    this._lastPongAt = 0;

    // Auto-reconnect state
    this._reconnectAttempt = 0;
    this._reconnectTimer = null;
    this._reconnectMax = 10;
    this._nonRecoverable = false;
    this._wasConnected = false;    // true after first successful DataChannel open
    this._lastSessionId = null;    // for silent auto-rejoin after reconnect
    this._autoRejoining = false;   // suppresses session_info output during rejoin

    // Last tool tracking
    this._lastToolName = '';
    this._lastToolArgs = {};

    // Tool confirmation picker state
    this.inToolConfirm = false;
    this.toolConfirmCall = null;
    this._lastExtractedCommands = [];

    // Session picker state
    this.inSessionPicker = false;
    this.pickerSessions = [];

    // User prompt state
    this.inUserPrompt = false;
    this.userPromptId = null;
    this.userPromptOptions = [];
    this.userPromptMulti = false;
    this.userPromptIdx = 0;
    this.userPromptSelected = [];

    // Active subagent tracking
    this._activeSubagents = new Set();

    // Interrupt warning state (double-Enter to interrupt LLM)
    this._interruptPendingText = null;
    this._interruptWarningStart = null;
    this._interruptWarningTimer = null;

    // Screen dimensions
    this._cols = term.cols || 80;
    this._rows = term.rows || 24;

    // Touch scroll state
    this._touchStartY = null;

    this._bindDomInput();
    this._bindTouchScroll();
    this._bindResize();

    // Prevent xterm from swallowing keyboard events (arrow keys, etc.)
    // All input goes through the DOM <input> field instead.
    this.term.attachCustomKeyEventHandler(() => false);

    // Redirect focus from xterm's hidden textarea to our input field
    // so keyboard always targets the DOM input on laptops/desktops.
    const xtermTextarea = this.term.element?.querySelector('.xterm-helper-textarea');
    if (xtermTextarea) {
      xtermTextarea.addEventListener('focus', () => this._inputField.focus());
    }
  }

  // -------------------------------------------------------------------------
  // Screen geometry
  // -------------------------------------------------------------------------

  _outputAreaHeight() {
    return Math.max(1, this._rows);
  }

  // -------------------------------------------------------------------------
  // Output buffer
  // -------------------------------------------------------------------------

  _pushLine(text) {
    this._outputLines.push(text);
    this._scrollOffset = 0;
  }

  _pushLines(lines) {
    for (const l of lines) this._outputLines.push(l);
    this._scrollOffset = 0;
  }

  _popLines(n) {
    const count = Math.min(n, this._outputLines.length);
    this._outputLines.splice(this._outputLines.length - count, count);
  }

  _scrollUp(n) {
    const max = Math.max(0, this._outputLines.length - this._outputAreaHeight());
    this._scrollOffset = Math.min(this._scrollOffset + n, max);
  }

  _scrollDown(n) {
    this._scrollOffset = Math.max(0, this._scrollOffset - n);
  }

  // -------------------------------------------------------------------------
  // Full repaint
  // -------------------------------------------------------------------------

  _fullRepaint() {
    this._drawOutputArea();
  }

  _drawOutputArea() {
    const height = this._outputAreaHeight();
    const total = this._outputLines.length;
    const end = total - this._scrollOffset;
    const start = Math.max(0, end - height);
    const w = this._cols;

    // Collect wrapped visual rows from the visible output lines
    const visualRows = [];
    for (let lineIdx = start; lineIdx < end; lineIdx++) {
      const line = this._outputLines[lineIdx].replace(/\x00STREAM\x00/g, '');
      const wrapped = wrapVisible(line, w);
      for (const wr of wrapped) {
        visualRows.push(wr);
      }
    }

    // Only show the last `height` visual rows (scroll to bottom)
    const vrStart = Math.max(0, visualRows.length - height);
    for (let row = 0; row < height; row++) {
      this.term.write(`\x1b[${row + 1};1H\x1b[2K`); // move to row, clear line
      const vrIdx = vrStart + row;
      if (vrIdx < visualRows.length) {
        this.term.write(visualRows[vrIdx]);
      }
    }

    // Scroll indicator
    if (this._scrollOffset > 0) {
      const indicator = ` ↑ ${this._scrollOffset} more `;
      const col = Math.max(1, this._cols - indicator.length - 1);
      this.term.write(`\x1b[1;${col}H${C.bgGray}${C.white}${indicator}${C.reset}`);
    }
  }

  // -------------------------------------------------------------------------
  // DOM: Input prompt label (updates color/text based on input prefix)
  // -------------------------------------------------------------------------

  _updatePromptLabel() {
    const val = this._inputField.value;
    const isSlash = val.startsWith('/');
    const isShell = val.startsWith('!');
    if (isSlash) {
      this._inputPrompt.textContent = 'cmd-> ';
      this._inputPrompt.style.color = 'var(--yellow)';
    } else if (isShell) {
      this._inputPrompt.textContent = 'shell>';
      this._inputPrompt.style.color = 'var(--magenta)';
    } else {
      this._inputPrompt.textContent = 'bear> ';
      this._inputPrompt.style.color = 'var(--cyan)';
    }
  }

  // -------------------------------------------------------------------------
  // DOM: Slash command dropdown
  // -------------------------------------------------------------------------

  _updateSlashDropdown() {
    const val = this._inputField.value;
    if (!val.startsWith('/') || this.slashCommands.length === 0) {
      this._hideSlashDropdown();
      return;
    }
    const matches = matchingSlashCommands(val, this.slashCommands);
    if (matches.length === 0) {
      this._hideSlashDropdown();
      return;
    }
    if (this._dropdownIdx >= matches.length) {
      this._dropdownIdx = matches.length - 1;
    }
    let html = '';
    for (let i = 0; i < matches.length; i++) {
      const cls = i === this._dropdownIdx ? 'dd-item active' : 'dd-item';
      html += `<div class="${cls}" data-idx="${i}">` +
        `<span class="dd-cmd">${this._esc(matches[i].cmd)}</span>` +
        `<span class="dd-desc">${this._esc(matches[i].desc)}</span>` +
        `</div>`;
    }
    this._slashDropdown.innerHTML = html;
    this._slashDropdown.style.display = 'block';
    this.fitAddon.fit();
  }

  _hideSlashDropdown() {
    if (this._slashDropdown.style.display !== 'none') {
      this._slashDropdown.style.display = 'none';
      this._slashDropdown.innerHTML = '';
      this._dropdownIdx = -1;
      this.fitAddon.fit();
    }
  }

  _acceptDropdown() {
    const val = this._inputField.value;
    const matches = matchingSlashCommands(val, this.slashCommands);
    const idx = this._dropdownIdx >= 0 ? this._dropdownIdx : 0;
    if (idx < matches.length) {
      this._inputField.value = matches[idx].cmd + ' ';
      this._updatePromptLabel();
    }
    this._hideSlashDropdown();
  }

  // -------------------------------------------------------------------------
  // DOM: Status bar
  // -------------------------------------------------------------------------

  _drawStatusBar() {
    const remainingMs = this._interruptWarningRemainingMs();
    if (remainingMs > 0) {
      this._statusBar.classList.add('warning');
      this._statusLeft.innerHTML = '<span>LLM is busy \u2014 tap Send again to interrupt</span>';
      this._statusRight.textContent = '';
    } else if (this._nonRecoverable) {
      this._statusBar.classList.add('warning');
      this._statusLeft.innerHTML = '<span>Connection lost</span>';
      this._statusRight.textContent = '';
    } else {
      this._statusBar.classList.remove('warning');
      // Restore spinner + session-name children if innerHTML destroyed them
      if (!this._statusLeft.querySelector('.spinner')) {
        this._statusLeft.innerHTML = '<span class="spinner">\u00b7\u00b7\u00b7\u00b7\u00b7</span><span class="session-name">bear</span>';
        this._statusSpinner = this._statusLeft.querySelector('.spinner');
        this._statusSession = this._statusLeft.querySelector('.session-name');
      }
      if (this._reconnectAttempt > 0) {
        this._statusSpinner.textContent = SPINNER[this._spinnerFrame % SPINNER.length];
        this._statusSession.textContent = `Reconnecting\u2026 (${this._reconnectAttempt}/${this._reconnectMax})`;
        if (!this._spinnerTimer) this._startSpinner();
      } else {
        const spinner = this._streaming
          ? SPINNER[this._spinnerFrame % SPINNER.length]
          : '\u00b7\u00b7\u00b7\u00b7\u00b7';
        const session = this._sessionName || 'bear';
        const subagentInfo = this._activeSubagents.size > 0
          ? `  \uD83D\uDD0D${this._activeSubagents.size}`
          : '';
        this._statusSpinner.textContent = spinner;
        this._statusSession.textContent = session + subagentInfo;
      }
      this._statusRight.textContent = '';
    }
  }

  _interruptWarningRemainingMs() {
    if (!this._interruptWarningStart) return 0;
    const elapsed = Date.now() - this._interruptWarningStart;
    return elapsed >= 6000 ? 0 : 6000 - elapsed;
  }

  _dismissInterruptWarning() {
    this._interruptPendingText = null;
    this._interruptWarningStart = null;
    if (this._interruptWarningTimer) {
      clearInterval(this._interruptWarningTimer);
      this._interruptWarningTimer = null;
    }
    this._drawStatusBar();
  }

  // -------------------------------------------------------------------------
  // Spinner
  // -------------------------------------------------------------------------

  _startSpinner() {
    if (this._spinnerTimer) return;
    this._spinnerTimer = setInterval(() => {
      this._spinnerFrame = (this._spinnerFrame + 1) % SPINNER.length;
      this._drawStatusBar();
    }, 100);
  }

  _stopSpinner() {
    if (this._spinnerTimer) {
      clearInterval(this._spinnerTimer);
      this._spinnerTimer = null;
    }
    this._drawStatusBar();
  }

  // HTML escaping helper
  _esc(s) {
    return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }

  // -------------------------------------------------------------------------
  // Resize
  // -------------------------------------------------------------------------

  _bindResize() {
    this.term.onResize(({ cols, rows }) => {
      this._cols = cols;
      this._rows = rows;
      this._fullRepaint();
    });

    // iOS keyboard / rotation: use visualViewport to resize the layout
    // so the browser never adds its own scrollbar.
    const vv = window.visualViewport;
    if (vv) {
      let resizeRaf = null;
      const onViewportResize = () => {
        if (resizeRaf) return;
        resizeRaf = requestAnimationFrame(() => {
          resizeRaf = null;
          const h = vv.height;
          document.documentElement.style.height = h + 'px';
          document.body.style.height = h + 'px';
          // Scroll the page back to top in case iOS shifted it
          window.scrollTo(0, 0);
          this.fitAddon.fit();
        });
      };
      vv.addEventListener('resize', onViewportResize);
      vv.addEventListener('scroll', () => window.scrollTo(0, 0));
    }
  }

  // -------------------------------------------------------------------------
  // Boot
  // -------------------------------------------------------------------------

  async boot() {
    this._pushLine('');
    this._pushLine(`${C.yellow}  ⠀⠀⠀⠀⠀⠀⠀⣤⠶⣤⣤⣤⡴⢦⡄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀${C.reset}`);
    this._pushLine(`${C.yellow}  ⠀⠀⠀⠀⠀⠀⠀⢷⠉⠀⠀⠀⠈⠁⢷⠖⠒⠲⠶⢤⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀${C.reset}`);
    this._pushLine(`${C.yellow}  ⠀⠀⠀⠀⠀⠀⠀⢸⠀⠠⠀⠠⠀⢠⠀⢳⠀⠀⠀⠀⠉⢳⡄⠀⠀⠀⠀⠀⠀⠀${C.reset}`);
    this._pushLine(`${C.yellow}  ⠀⠀⠀⠀⠀⠀⠀⠈⣧⡀⣶⡆⣠⠏⠀⠀⠀⠀⠀⠀⠀⠀⢹⡄⠀⠀⠀⠀⠀⠀${C.reset}`);
    this._pushLine(`${C.yellow}  ⠀⠀⠀⠀⠀⠀⠀⠀⢸⡉⠒⠚⠁⠀⠀⠀⢀⠀⠀⠀⠀⠀⠀⡇⠀⠀⠀⠀⠀⠀${C.reset}`);
    this._pushLine(`${C.yellow}  ⠀⠀⠀⠀⠀⠀⠀⠀⠸⡇⢢⡀⣤⠀⠀⠀⢸⠀⠀⠀⠀⠀⢀⡇⠀⠀⠀⠀⠀⠀${C.reset}`);
    this._pushLine(`${C.yellow}  ⠀⠀⠀⠀⠀⠀⠀⠀⠀⣷⠀⠉⢻⡀⠀⠀⣾⠤⠤⡄⠀⠀⢸⠁⠀⠀⠀⠀⠀⠀${C.reset}`);
    this._pushLine(`${C.yellow}  ⠀⠀⠀⠀⠀⠀⠀⠀⠀⣸⠄⠀⣼⡇⠀⢠⡇⢀⡼⣻⠀⢀⡟⠀⠀⠀⠀⠀⠀⠀${C.reset}`);
    this._pushLine(`${C.yellow}  ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠛⠒⠚⠙⠷⠶⠞⠉⠉⠀⠓⠒⠚⠁⠀⠀⠀⠀⠀${C.reset}`);
    this._pushLine('');
    this._pushLine(`${C.bold}${C.cyan}    Welcome to Bear coding agent${C.reset}`);
    this._pushLine(`${C.gray}    v${BEAR_VERSION}  •  Type /help for commands${C.reset}`);
    this._pushLine('');
    this._fullRepaint();

    if (!RELAY_ROOM) {
      this._pushLine(`${C.red}  Relay not configured. Set BEAR_ROOM_ID.${C.reset}`);
      this._fullRepaint();
      return;
    }

    this._connectRelay();
  }

  // -------------------------------------------------------------------------
  // Session picker
  // -------------------------------------------------------------------------

  _showSessionPicker() {
    // Request session list over the DataChannel; the response arrives
    // as a session_list_result message and is handled in _handleServerMessage.
    this._sendJson({ type: 'session_list' });
  }

  _showSessionPickerUI() {
    this.inSessionPicker = true;
    this.pickerIdx = 0;
    this._pushLine(`${C.bold}${C.white}  Select a session:${C.reset}`);
    this._fullRepaint();
    this._renderSessionPicker();
  }

  _renderSessionPicker() {
    const items = [
      { label: '+ New Session', detail: '' },
      ...this.pickerSessions.map(s => ({
        label: s.name || s.id.substring(0, 8) + '\u2026',
        detail: s.cwd,
      })),
    ];
    let html = '<div class="picker-title">Select a session</div>';
    for (let i = 0; i < items.length; i++) {
      const cls = i === this.pickerIdx ? 'picker-item active' : 'picker-item';
      const ind = i === this.pickerIdx ? '\u276F' : ' ';
      const det = items[i].detail
        ? `<span class="pi-detail">${this._esc(items[i].detail)}</span>` : '';
      html += `<div class="${cls}" data-idx="${i}">` +
        `<span class="pi-indicator">${ind}</span>` +
        `<span class="pi-label">${this._esc(items[i].label)}</span>${det}</div>`;
    }
    html += '<div class="picker-hint">Tap to select</div>';
    this._pickerOverlay.innerHTML = html;
    this._pickerOverlay.style.display = 'block';
    this.fitAddon.fit();
    // Bind tap/click
    this._pickerOverlay.querySelectorAll('.picker-item').forEach(el => {
      el.addEventListener('click', () => {
        this.pickerIdx = parseInt(el.dataset.idx);
        this._pickerSelectSession();
      });
    });
  }

  _pickerSelectSession() {
    this.inSessionPicker = false;
    this._hidePicker();

    if (this.pickerIdx === 0) {
      this._pushLine(`${C.gray}  Creating new session\u2026${C.reset}`);
      this._fullRepaint();
      this._sendJson({ type: 'session_create', cwd: null });
    } else {
      const session = this.pickerSessions[this.pickerIdx - 1];
      this._pushLine(`${C.gray}  Connecting to session ${session.id.substring(0, 8)}\u2026${C.reset}`);
      this._fullRepaint();
      this._sendJson({ type: 'session_select', session_id: session.id });
    }
  }

  _hidePicker() {
    this._pickerOverlay.style.display = 'none';
    this._pickerOverlay.innerHTML = '';
    this.fitAddon.fit();
  }

  // -------------------------------------------------------------------------
  // ICE server configuration (STUN + dynamic TURN fetch)
  // -------------------------------------------------------------------------

  async _buildIceServers() {
    const stun = [...ICE_STUN];
    let turn = [];
    try {
      const res = await fetch(`${PUBLIC_URL}/api/signal/turn-credentials`, {
        credentials: 'same-origin',
      });
      if (res.ok) {
        const data = await res.json();
        if (Array.isArray(data.turn_servers) && data.turn_servers.length > 0) {
          turn = data.turn_servers;
        }
      }
    } catch (_) {
      // Fetch failed — fall through to fallback
    }
    if (turn.length === 0 && ICE_FALLBACK_TURN.length > 0) {
      turn = ICE_FALLBACK_TURN;
    }
    if (turn.length > 0) {
      console.log(`[bear] TURN servers: ${turn.map(t => t.urls).flat().join(', ')}`);
    } else {
      console.warn('[bear] No TURN servers available — mobile/symmetric NAT connections may fail');
    }
    return [...stun, ...turn];
  }

  // -------------------------------------------------------------------------
  // WebRTC DataChannel connection
  // -------------------------------------------------------------------------

  async _connectRelay() {
    this._cancelReconnect();
    this.inToolConfirm = false;
    this.toolConfirmCall = null;
    this.inSessionPicker = false;
    this.inUserPrompt = false;
    this._activeSubagents = new Set();
    this._hidePicker();

    this._cleanup();

    if (this._reconnectAttempt === 0) {
      this._pushLine(`${C.gray}  Connecting via WebRTC…${C.reset}`);
      this._fullRepaint();
    }
    this._drawStatusBar();

    // Fetch TURN credentials dynamically; fall back to page-injected globals
    const iceServers = await this._buildIceServers();
    const hasTurn = iceServers.some(s => {
      const u = Array.isArray(s.urls) ? s.urls : [s.urls];
      return u.some(x => x.startsWith('turn'));
    });
    if (!hasTurn && this._reconnectAttempt === 0) {
      this._pushLine(`${C.yellow}  ⚠ No TURN servers — mobile connections may fail${C.reset}`);
      this._fullRepaint();
    }

    this.pc = new RTCPeerConnection({ iceServers });

    this.pc.addEventListener('icecandidateerror', (e) => {
      const { errorCode, errorText, url } = e;
      console.warn(`[bear] ICE candidate error: ${url} code=${errorCode} ${errorText}`);
    });

    this.dc = this.pc.createDataChannel('bear', { ordered: true });

    this.dc.onopen = () => {
      this._reconnectAttempt = 0;
      this._nonRecoverable = false;
      const wasReconnect = this._wasConnected;
      this._wasConnected = true;
      this._startHeartbeat();
      // DataChannel is open — lobby session list request is deferred
      // until the server sends slash_commands (its "lobby ready" signal),
      // ensuring the server's on_message handler is registered first.
      this._lobbyPending = true;
      // If reconnecting and we have a previous session, auto-rejoin silently
      if (wasReconnect && this._lastSessionId) {
        this._autoRejoining = true;
      }
      this._drawStatusBar();
    };

    this.dc.onclose = () => {
      this._stopSpinner();
      if (this._wasConnected && !this._nonRecoverable) {
        this._cleanup();
        this._scheduleReconnect();
      } else {
        this._pushLine(`${C.gray}  Disconnected.${C.reset}`);
        this._fullRepaint();
      }
    };

    this.dc.onerror = (e) => {
      console.warn(`[bear] DataChannel error: ${e.error?.message || 'unknown'}`);
    };

    this.dc.onmessage = (event) => {
      let msg;
      try { msg = JSON.parse(event.data); } catch { return; }

      // Reassemble chunked messages from the server
      if (msg.__chunk) {
        if (!this._chunkBufs) this._chunkBufs = {};
        // Evict stale chunk buffers (incomplete for >30s)
        const now = Date.now();
        for (const id of Object.keys(this._chunkBufs)) {
          if (now - this._chunkBufs[id].createdAt > 30000) {
            delete this._chunkBufs[id];
          }
        }
        const buf = this._chunkBufs[msg.id] || (this._chunkBufs[msg.id] = { parts: [], total: msg.total, createdAt: now });
        buf.parts[msg.idx] = msg.data;
        const received = buf.parts.filter(p => p !== undefined).length;
        if (received < buf.total) return; // still waiting for more chunks
        const full = buf.parts.join('');
        delete this._chunkBufs[msg.id];
        try { msg = JSON.parse(full); } catch { return; }
      }

      this._handleServerMessage(msg);
    };

    this._pendingIceCandidates = [];
    this.pc.onicecandidate = (event) => {
      if (!event.candidate) return;
      console.log(`[bear] ICE candidate: ${event.candidate.type || 'unknown'} ${event.candidate.candidate}`);

      const c = {
        candidate: event.candidate.candidate,
        sdpMid: event.candidate.sdpMid,
        sdpMLineIndex: event.candidate.sdpMLineIndex,
      };
      if (this._connId) {
        this._postIceCandidates([c]);
      } else {
        // Buffer until _connId is available (set after offer POST returns)
        this._pendingIceCandidates.push(c);
      }
    };

    this.pc.onconnectionstatechange = () => {
      const state = this.pc?.connectionState;
      if (state === 'failed' || state === 'disconnected') {
        this._stopSpinner();
        if (this._wasConnected && !this._nonRecoverable) {
          this._cleanup();
          this._scheduleReconnect();
        } else {
          this._pushLine(`${C.red}  Connection lost.${C.reset}`);
          this._fullRepaint();
        }
      }
    };

    this._doRelaySignaling();
  }

  async _doRelaySignaling() {
    try {
      const offer = await this.pc.createOffer();
      await this.pc.setLocalDescription(offer);

      // POST offer to public server (proxied to relay internal API)
      const offerRes = await fetch(`${PUBLIC_URL}/api/signal/${RELAY_ROOM}/offer`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        credentials: 'same-origin',
        body: JSON.stringify({ sdp: offer.sdp }),
      });

      if (!offerRes.ok) {
        if (offerRes.status === 401 || offerRes.status === 403 || offerRes.status === 404) {
          this._nonRecoverable = true;
          this._showRetryOverlay(`Signaling failed: ${offerRes.status} — room may be revoked or not found.`);
          this._drawStatusBar();
        } else {
          this._cleanup();
          this._scheduleReconnect();
        }
        return;
      }

      const offerData = await offerRes.json();
      this._connId = offerData.conn_id;

      // Flush any ICE candidates buffered before _connId was available
      if (this._pendingIceCandidates.length > 0) {
        this._postIceCandidates(this._pendingIceCandidates);
        this._pendingIceCandidates = [];
      }

      // Poll public server for answer (proxied from relay internal API)
      const deadline = Date.now() + 30000;
      while (Date.now() < deadline) {
        const ansRes = await fetch(`${PUBLIC_URL}/api/signal/${RELAY_ROOM}/answer/${this._connId}`, {
          credentials: 'same-origin',
        });
        if (ansRes.status === 200) {
          const ansData = await ansRes.json();
          // Extract client JWT for direct ICE exchange with relay
          if (ansData.client_jwt) {
            this._relayJwt = ansData.client_jwt;
          }
          await this.pc.setRemoteDescription(
            new RTCSessionDescription({ type: 'answer', sdp: ansData.sdp })
          );
          await this._showSasCode();
          this._startRelayIcePoll();
          return;
        }
        await new Promise(r => setTimeout(r, 500));
      }
      // Timeout — recoverable
      console.warn('[bear] Relay signaling timeout: no answer received');
      this._cleanup();
      this._scheduleReconnect();
    } catch (e) {
      // Network error — recoverable
      console.warn(`[bear] Relay signaling error: ${e.message}`);
      this._cleanup();
      this._scheduleReconnect();
    }
  }

  async _showSasCode() {
    try {
      const localSdp = this.pc.localDescription?.sdp;
      const remoteSdp = this.pc.remoteDescription?.sdp;
      if (!localSdp || !remoteSdp) return;

      const extractFp = (sdp) => {
        const m = sdp.match(/a=fingerprint:sha-256 ([^\r\n]+)/);
        return m ? m[1].trim() : null;
      };
      const localFp = extractFp(localSdp);
      const remoteFp = extractFp(remoteSdp);
      if (!localFp || !remoteFp) return;

      const sorted = [localFp, remoteFp].sort();
      const input = new TextEncoder().encode(sorted[0] + ':' + sorted[1]);
      const hash = await crypto.subtle.digest('SHA-256', input);
      const bytes = new Uint8Array(hash);
      const sas = [bytes[0], bytes[1], bytes[2]]
        .map(b => b.toString(16).padStart(2, '0'))
        .join('')
        .toUpperCase();
      this._pushLine(`  Verification: ${sas}`);
      this._fullRepaint();
    } catch { /* ignore */ }
  }

  _postIceCandidates(candidates) {
    fetch(`${PUBLIC_URL}/api/signal/${RELAY_ROOM}/ice/${this._connId}/client`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      credentials: 'same-origin',
      body: JSON.stringify({ candidates }),
    }).catch(() => {});
  }

  _startRelayIcePoll() {
    this._stopIcePoll();
    this._icePollTimer = setInterval(async () => {
      if (!this._connId) return;
      try {
        const res = await fetch(`${PUBLIC_URL}/api/signal/${RELAY_ROOM}/ice/${this._connId}/server`, {
          credentials: 'same-origin',
        });
        if (!res.ok) return;
        const data = await res.json();
        const cands = data.candidates || [];
        for (const c of cands) {
          if (typeof c === 'string') {
            await this.pc.addIceCandidate(new RTCIceCandidate({ candidate: c }));
          } else if (c && c.candidate) {
            await this.pc.addIceCandidate(new RTCIceCandidate({
              candidate: c.candidate,
              sdpMid: c.sdpMid ?? null,
              sdpMLineIndex: c.sdpMLineIndex ?? null,
            }));
          }
        }
      } catch { /* ignore */ }
    }, 200);
    setTimeout(() => this._stopIcePoll(), 30000);
  }

  _stopIcePoll() {
    if (this._icePollTimer) {
      clearInterval(this._icePollTimer);
      this._icePollTimer = null;
    }
  }

  _cleanup() {
    this._stopIcePoll();
    this._stopSpinner();
    this._stopHeartbeat();
    this._dismissInterruptWarning();
    if (this.dc) { try { this.dc.close(); } catch {} this.dc = null; }
    if (this.pc) { try { this.pc.close(); } catch {} this.pc = null; }
    this._connId = null;
    this._chunkBufs = {};
  }

  _isConnected() {
    return this.dc && this.dc.readyState === 'open';
  }

  // -------------------------------------------------------------------------
  // Auto-reconnect
  // -------------------------------------------------------------------------

  _scheduleReconnect() {
    if (this._reconnectTimer) return; // already scheduled
    if (this._nonRecoverable) {
      this._showRetryOverlay('Connection lost — unable to reach server.');
      this._drawStatusBar();
      return;
    }
    if (this._reconnectAttempt >= this._reconnectMax) {
      this._nonRecoverable = true;
      this._showRetryOverlay('Connection lost — max retries exceeded.');
      this._drawStatusBar();
      return;
    }
    const delay = Math.min(1000 * Math.pow(2, this._reconnectAttempt), 30000);
    this._reconnectAttempt++;
    console.log(`[bear] Reconnecting in ${delay}ms (attempt ${this._reconnectAttempt}/${this._reconnectMax})`);
    this._drawStatusBar();
    this._reconnectTimer = setTimeout(() => {
      this._reconnectTimer = null;
      this._connectRelay();
    }, delay);
  }

  _cancelReconnect() {
    if (this._reconnectTimer) {
      clearTimeout(this._reconnectTimer);
      this._reconnectTimer = null;
    }
  }

  _showRetryOverlay(message) {
    let html = '<div class="picker-title">' + this._esc(message) + '</div>';
    html += `<div class="picker-item" data-idx="0" style="cursor:pointer">` +
      `<span class="pi-indicator">↻</span>` +
      `<span class="pi-label">Reconnect</span></div>`;
    this._pickerOverlay.innerHTML = html;
    this._pickerOverlay.style.display = 'block';
    this.fitAddon.fit();
    this._pickerOverlay.querySelectorAll('.picker-item').forEach(el => {
      el.addEventListener('click', () => {
        this._hidePicker();
        this._nonRecoverable = false;
        this._reconnectAttempt = 0;
        this._connectRelay();
      });
    });
  }

  // -------------------------------------------------------------------------
  // Server message dispatch
  // -------------------------------------------------------------------------

  _handleServerMessage(msg) {
    switch (msg.type) {
      case 'slash_commands':
        this.slashCommands = Array.isArray(msg.commands) ? msg.commands : [];
        this._updateSlashDropdown();
        // slash_commands is the server's "lobby ready" signal — now safe
        // to request the session list over the DataChannel.
        if (this._lobbyPending) {
          this._lobbyPending = false;
          if (this._autoRejoining && this._lastSessionId) {
            // Silent auto-rejoin: skip session picker, go straight to last session
            this._sendJson({ type: 'session_select', session_id: this._lastSessionId });
          } else {
            this._showSessionPicker();
          }
        }
        break;

      case 'session_list_result': {
        // If we get a session list while auto-rejoining, the session_select
        // failed (session was deleted). Fall back to normal session picker.
        if (this._autoRejoining) {
          this._autoRejoining = false;
          this._lastSessionId = null;
        }
        // Lobby response: populate session picker with received sessions
        const sessions = Array.isArray(msg.sessions) ? msg.sessions : [];
        if (sessions.length === 0) {
          // No existing sessions — auto-create one (matches native bear behaviour)
          this._pushLine(`${C.gray}  Creating new session…${C.reset}`);
          this._fullRepaint();
          this._sendJson({ type: 'session_create', cwd: null });
        } else {
          this.pickerSessions = sessions;
          this._showSessionPickerUI();
        }
        break;
      }

      case 'session_created':
        // Server created and auto-bound to the new session;
        // session_info will follow from the bound relay loop.
        break;

      case 'session_info':
        this._sessionName = msg.session.name || msg.session.id.substring(0, 8);
        this._sessionCwd = msg.session.cwd || '';
        this._lastSessionId = msg.session.id;
        if (this._autoRejoining) {
          this._autoRejoining = false;
          this._pushLine(`${C.gray}  Reconnected.${C.reset}`);
        } else {
          this._pushLine('');
          this._pushLine(`${C.cyan}  Connected to session ${C.bold}${msg.session.id}${C.reset}`);
          this._pushLine(`${C.gray}  Working directory: ${msg.session.cwd}${C.reset}`);
          this._pushLine(`${C.gray}  Type /help for commands${C.reset}`);
          this._pushLine('');
        }
        this._fullRepaint();
        break;

      case 'assistant_text':
        // Remove the "Thinking…" line if present (before streaming check)
        if (this._thinkingLineShown) {
          this._popLines(1);
          this._thinkingLineShown = false;
        }
        if (!this._streaming) {
          this._streaming = true;
          this._streamBuf = '';
          this._startSpinner();
        }
        this._streamBuf += msg.text;
        this._flushStreamToOutput();
        this._fullRepaint();
        break;

      case 'assistant_text_done':
        if (this._streaming) {
          this._streaming = false;
          this._flushStreamToOutput();
          this._streamBuf = '';
          this._pushLine('');
          this._stopSpinner();
        }
        this._dismissInterruptWarning();
        this._fullRepaint();
        break;

      case 'tool_request': {
        const tc = msg.tool_call;
        this._lastToolName = tc.name;
        this._lastToolArgs = tc.arguments;
        const cmds = msg.extracted_commands || [this._extractBaseCommand(tc)];
        this._lastExtractedCommands = cmds;

        // Show tool card
        const descLines = formatToolDescription(tc.name, tc.arguments || {});
        this._pushLine(`${C.gray}  ┌─ ${C.magenta}⚡ ${tc.name}${C.gray} ─${C.reset}`);
        for (const line of descLines) {
          this._pushLine(`${C.gray}  │  ${C.white}${line}${C.reset}`);
        }
        this._pushLine(`${C.gray}  └─${C.reset}`);

        // Enter picker mode
        this.toolConfirmCall = tc;
        this._tcIdx = 0;
        this.inToolConfirm = true;
        this._playAlert();
        this._renderToolConfirm();
        break;
      }

      case 'tool_output':
        this._lastToolName = msg.tool_name || this._lastToolName || '';
        this._lastToolArgs = msg.tool_args || this._lastToolArgs || {};
        this._renderToolOutput(this._lastToolName, this._lastToolArgs, msg.output);
        this._fullRepaint();
        break;

      case 'process_started':
        this._pushLine(`${C.magenta}  [proc] Started pid=${msg.info.pid} cmd=${msg.info.command}${C.reset}`);
        this._fullRepaint();
        break;

      case 'process_output':
        this._pushLine(`${C.magenta}  [${msg.pid}] ${msg.text}${C.reset}`);
        this._fullRepaint();
        break;

      case 'process_exited': {
        const code = msg.code !== null && msg.code !== undefined ? msg.code : 'unknown';
        this._pushLine(`${C.magenta}  [proc] Process ${msg.pid} exited (code ${code})${C.reset}`);
        this._fullRepaint();
        break;
      }

      case 'process_list_result':
        if (msg.processes.length === 0) {
          this._pushLine(`${C.gray}  No background processes.${C.reset}`);
        } else {
          this._pushLine(`${C.white}  Background processes:${C.reset}`);
          for (const p of msg.processes) {
            const status = p.running ? 'running' : 'exited';
            this._pushLine(`${C.gray}    pid=${p.pid} [${status}] ${p.command}${C.reset}`);
          }
        }
        this._fullRepaint();
        break;

      case 'session_renamed':
        this._sessionName = msg.name;
        this._pushLine(`${C.green}  Session renamed to: ${msg.name}${C.reset}`);
        this._fullRepaint();
        break;

      case 'client_state':
        if (Array.isArray(msg.input_history)) {
          this.history = msg.input_history;
          this.historyIdx = -1;
        }
        break;

      case 'tool_auto_approved': {
        const tc = msg.tool_call;
        this._lastToolName = tc.name;
        this._lastToolArgs = tc.arguments || {};
        const descLines = formatToolDescription(tc.name, tc.arguments || {});
        this._pushLine(`${C.gray}  ┌─ ⚡ ${tc.name} ─ (auto-approved)${C.reset}`);
        for (const line of descLines) {
          this._pushLine(`${C.gray}  │  ${line}${C.reset}`);
        }
        this._pushLine(`${C.gray}  └─${C.reset}`);
        this._fullRepaint();
        break;
      }

      case 'tool_resolved':
        // Another client resolved the tool confirmation — dismiss our picker
        if (this.inToolConfirm && this.toolConfirmCall && this.toolConfirmCall.id === msg.tool_call_id) {
          this.inToolConfirm = false;
          this.toolConfirmCall = null;
          this._hidePicker();
          const label = msg.approved
            ? `${C.green}  ✓ Approved (by another client)${C.reset}`
            : `${C.red}  ✗ Denied (by another client)${C.reset}`;
          this._pushLine(label);
          this._pushLine('');
          this._fullRepaint();
        }
        break;

      case 'prompt_resolved': {
        // Another client resolved a user prompt or task plan — dismiss our picker.
        // Task plan prompts use `__taskplan__<plan_id>` as the userPromptId.
        const matchesPrompt = this.inUserPrompt && (
          this.userPromptId === msg.prompt_id ||
          this.userPromptId === `__taskplan__${msg.prompt_id}`
        );
        if (matchesPrompt) {
          this.inUserPrompt = false;
          this._hidePicker();
          this._pushLine(`${C.gray}  (resolved by another client)${C.reset}`);
          this._pushLine('');
          this._fullRepaint();
        }
        break;
      }

      case 'user_input':
        if (this._awaitingInputEcho) {
          // Our own echo — already rendered locally in _submitInput()
          this._awaitingInputEcho = false;
        } else {
          // Another client submitted this prompt
          const uiPrompt = msg.text.startsWith('/') ? 'cmd-> ' : msg.text.startsWith('!') ? 'shell>' : 'bear> ';
          this._pushLine(`  ${C.bold}${C.white}${uiPrompt}${C.reset}${C.white}${msg.text}${C.reset}`);
          this._fullRepaint();
        }
        break;

      case 'notice':
        this._pushLine(`${C.yellow}[notice] ${msg.text}${C.reset}`);
        this._fullRepaint();
        break;

      case 'error':
        this._pushLine(`${C.red}[error] ${msg.text}${C.reset}`);
        this._fullRepaint();
        break;

      case 'thinking':
        if (!this._thinkingLineShown) {
          this._streaming = true;
          this._streamBuf = '';
          this._pushLine(`${C.dim}${C.gray}  ⟳ Thinking…${C.reset}`);
          this._thinkingLineShown = true;
          this._startSpinner();
          this._fullRepaint();
        }
        break;

      case 'user_prompt':
        this.inUserPrompt = true;
        this.userPromptId = msg.prompt_id;
        this.userPromptOptions = msg.options;
        this.userPromptMulti = msg.multi;
        this.userPromptIdx = 0;
        this.userPromptSelected = new Array(msg.options.length).fill(false);
        this._pushLine(`${C.bold}${C.cyan}  ${msg.question}${C.reset}`);
        this._fullRepaint();
        this._playAlert();
        this._renderUserPrompt();
        break;

      case 'task_plan': {
        // Show proposed task plan and enter confirmation mode
        this._pushLine('');
        this._pushLine(`${C.bold}${C.cyan}  📋 Proposed task plan:${C.reset}`);
        for (const task of msg.tasks) {
          const tag = task.needs_write
            ? `${C.yellow}[write]${C.reset}`
            : `${C.green}[read]${C.reset}`;
          this._pushLine(`${C.gray}    ${task.id}. ${tag} ${C.white}${task.description}${C.reset}`);
        }
        this._pushLine('');
        // Reuse user prompt picker for approval
        this.inUserPrompt = true;
        this.userPromptId = `__taskplan__${msg.plan_id}`;
        this.userPromptOptions = ['Approve', 'Reject'];
        this.userPromptMulti = false;
        this.userPromptIdx = 0;
        this.userPromptSelected = [false, false];
        this._pushLine(`${C.bold}${C.cyan}  Execute this plan?${C.reset}`);
        this._fullRepaint();
        this._playAlert();
        this._renderUserPrompt();
        break;
      }

      case 'task_progress': {
        const icons = { pending: '○', in_progress: '→', completed: '✓', failed: '✗' };
        const colors = { pending: C.gray, in_progress: C.yellow, completed: C.green, failed: C.red };
        const icon = icons[msg.status] || '·';
        const color = colors[msg.status] || C.gray;
        const detail = msg.detail ? ` — ${msg.detail}` : '';
        this._pushLine(`  ${color}${icon} Task ${msg.task_id}${C.reset}${C.gray}${detail}${C.reset}`);
        this._fullRepaint();
        break;
      }

      case 'subagent_update': {
        // Track active subagent count
        if (msg.status === 'running') {
          this._activeSubagents.add(msg.subagent_id);
        } else if (msg.status === 'completed' || msg.status === 'failed') {
          this._activeSubagents.delete(msg.subagent_id);
        }
        const icons = { running: '🔍', completed: '✓', failed: '✗' };
        const colors = { running: C.cyan, completed: C.green, failed: C.red };
        const icon = icons[msg.status] || '·';
        const color = colors[msg.status] || C.gray;
        const detail = msg.detail ? ` → ${msg.detail}` : '';
        this._pushLine(`  ${icon} ${color}${msg.description}${C.reset}${C.gray}${detail}${C.reset}`);
        this._fullRepaint();
        break;
      }

      case 'pong':
        this._lastPongAt = Date.now();
        break;
    }
  }

  // -------------------------------------------------------------------------
  // Streaming buffer
  // -------------------------------------------------------------------------

  _flushStreamToOutput() {
    const TAG = '\x00STREAM\x00';
    // Remove previous stream lines (tagged)
    while (this._outputLines.length > 0 && this._outputLines[this._outputLines.length - 1].startsWith(TAG)) {
      this._outputLines.pop();
    }
    const lines = this._streamBuf.split('\n');
    let inThink = false;
    const mdState = { inCodeBlock: false };
    for (let i = 0; i < lines.length; i++) {
      const prefix = i === 0 ? '🐻 ' : '   ';
      const trimmed = lines[i].trim();

      // Track <think> blocks
      if (trimmed.startsWith('<think>') || trimmed.startsWith('<think ')) {
        inThink = true;
      }

      const isThought = inThink
        || trimmed.startsWith('THOUGHT:')
        || trimmed.startsWith('Thought:')
        || trimmed.startsWith('thought:');

      if (isThought) {
        this._outputLines.push(`${TAG}  ${prefix}${C.gray}${lines[i]}${C.reset}`);
      } else {
        // Render through markdown pipeline
        const mdLines = renderMdLine(lines[i], mdState);
        for (let j = 0; j < mdLines.length; j++) {
          const p = (i === 0 && j === 0) ? '🐻 ' : '   ';
          this._outputLines.push(`${TAG}  ${p}${mdLines[j]}`);
        }
      }

      if (trimmed.includes('</think>')) {
        inThink = false;
      }
    }
    this._scrollOffset = 0;
  }

  // -------------------------------------------------------------------------
  // DOM: Input binding (replaces _bindTerminal)
  // -------------------------------------------------------------------------

  _bindDomInput() {
    // --- Input field events ---
    this._inputField.addEventListener('input', () => {
      this._updatePromptLabel();
      this._updateSlashDropdown();
      if (this._interruptPendingText !== null) this._dismissInterruptWarning();
    });

    this._inputField.addEventListener('keydown', (e) => {
      // If a picker is active, route arrow keys to it
      if (this.inSessionPicker || this.inToolConfirm || this.inUserPrompt) {
        if (e.key === 'ArrowUp' || e.key === 'ArrowDown' || e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          this._handlePickerKeyboard(e.key);
          return;
        }
      }

      if (e.key === 'Enter') {
        e.preventDefault();
        this._handleEnter();
        return;
      }

      if (e.key === 'ArrowUp') {
        // Slash dropdown navigation
        if (this._slashDropdown.style.display === 'block') {
          e.preventDefault();
          const matches = matchingSlashCommands(this._inputField.value, this.slashCommands);
          if (matches.length > 0) {
            this._dropdownIdx = this._dropdownIdx <= 0 ? matches.length - 1 : this._dropdownIdx - 1;
            this._updateSlashDropdown();
          }
          return;
        }
        // History navigation
        e.preventDefault();
        this._historyUp();
        return;
      }

      if (e.key === 'ArrowDown') {
        if (this._slashDropdown.style.display === 'block') {
          e.preventDefault();
          const matches = matchingSlashCommands(this._inputField.value, this.slashCommands);
          if (matches.length > 0) {
            this._dropdownIdx = (this._dropdownIdx < 0 || this._dropdownIdx >= matches.length - 1) ? 0 : this._dropdownIdx + 1;
            this._updateSlashDropdown();
          }
          return;
        }
        e.preventDefault();
        this._historyDown();
        return;
      }

      if (e.key === 'Tab') {
        if (this._slashDropdown.style.display === 'block') {
          e.preventDefault();
          this._acceptDropdown();
        }
        return;
      }

      if (e.key === 'Escape') {
        this._inputField.value = '';
        this._updatePromptLabel();
        this._hideSlashDropdown();
        return;
      }

      // Page up/down for scrolling
      if (e.key === 'PageUp') { e.preventDefault(); this._scrollUp(5); this._fullRepaint(); return; }
      if (e.key === 'PageDown') { e.preventDefault(); this._scrollDown(5); this._fullRepaint(); return; }
    });

    // --- Send button ---
    this._sendBtn.addEventListener('click', () => {
      this._handleEnter();
      this._inputField.focus();
    });

    // --- Slash dropdown click ---
    this._slashDropdown.addEventListener('click', (e) => {
      const item = e.target.closest('.dd-item');
      if (item) {
        this._dropdownIdx = parseInt(item.dataset.idx);
        this._acceptDropdown();
        this._inputField.focus();
      }
    });

    // --- Mouse wheel scroll on xterm output ---
    const termEl = this.term.element;
    document.addEventListener('wheel', (e) => {
      if (!termEl.contains(e.target)) return;
      e.preventDefault();
      e.stopImmediatePropagation();
      if (e.deltaY < 0) {
        this._scrollUp(3);
      } else if (e.deltaY > 0) {
        this._scrollDown(3);
      }
      this._fullRepaint();
    }, { passive: false, capture: true });
  }

  _handleEnter() {
    // If slash dropdown is visible and an item is selected, accept it
    if (this._slashDropdown.style.display === 'block' && this._dropdownIdx >= 0) {
      this._acceptDropdown();
      return;
    }
    // Double-Enter interrupt: if warning is active, confirm the interrupt
    if (this._interruptPendingText !== null && this._interruptWarningRemainingMs() > 0) {
      const text = this._interruptPendingText;
      this._dismissInterruptWarning();
      this._awaitingInputEcho = true;
      this._sendJson({ type: 'input', text });
      this._fullRepaint();
      return;
    }
    // If LLM is busy and user typed non-empty text, show warning
    if (this._streaming && this._inputField.value.trim()) {
      this._submitInputToWarning();
      return;
    }
    // Normal submit
    this._dismissInterruptWarning();
    this._submitInput();
  }

  // Handle keyboard on active picker overlays (session, tool confirm, user prompt)
  _handlePickerKeyboard(key) {
    if (this.inSessionPicker) {
      const totalItems = this.pickerSessions.length + 1;
      if (key === 'ArrowUp' && this.pickerIdx > 0) {
        this.pickerIdx--;
        this._renderSessionPicker();
      } else if (key === 'ArrowDown' && this.pickerIdx < totalItems - 1) {
        this.pickerIdx++;
        this._renderSessionPicker();
      } else if (key === 'Enter') {
        this._pickerSelectSession();
      }
    } else if (this.inToolConfirm) {
      if (key === 'ArrowUp' && this._tcIdx > 0) {
        this._tcIdx--;
        this._renderToolConfirm();
      } else if (key === 'ArrowDown' && this._tcIdx < TOOL_CONFIRM_LABELS.length - 1) {
        this._tcIdx++;
        this._renderToolConfirm();
      } else if (key === 'Enter') {
        this._toolConfirmSelect(this._tcIdx);
      }
    } else if (this.inUserPrompt) {
      const total = this.userPromptOptions.length;
      if (key === 'ArrowUp' && this.userPromptIdx > 0) {
        this.userPromptIdx--;
        this._renderUserPrompt();
      } else if (key === 'ArrowDown' && this.userPromptIdx < total - 1) {
        this.userPromptIdx++;
        this._renderUserPrompt();
      } else if (key === ' ' && this.userPromptMulti) {
        this.userPromptSelected[this.userPromptIdx] = !this.userPromptSelected[this.userPromptIdx];
        this._renderUserPrompt();
      } else if (key === 'Enter') {
        this._userPromptSelect();
      }
    }
  }

  // -------------------------------------------------------------------------
  // DOM: Touch scroll on xterm output
  // -------------------------------------------------------------------------

  _bindTouchScroll() {
    const termEl = this.term.element;
    termEl.addEventListener('touchstart', (e) => {
      if (e.touches.length === 1) {
        this._touchStartY = e.touches[0].clientY;
      }
    }, { passive: false });

    termEl.addEventListener('touchmove', (e) => {
      if (this._touchStartY === null || e.touches.length !== 1) return;
      e.preventDefault(); // stop browser from scrolling the page
      const dy = this._touchStartY - e.touches[0].clientY;
      const threshold = 20; // pixels per scroll step
      if (Math.abs(dy) >= threshold) {
        const steps = Math.floor(Math.abs(dy) / threshold);
        if (dy > 0) {
          this._scrollDown(steps);
        } else {
          this._scrollUp(steps);
        }
        this._touchStartY = e.touches[0].clientY;
        this._fullRepaint();
      }
    }, { passive: false });

    termEl.addEventListener('touchend', () => {
      this._touchStartY = null;
    }, { passive: true });
  }

  // -------------------------------------------------------------------------
  // DOM: Tool confirmation picker
  // -------------------------------------------------------------------------

  _renderToolConfirm() {
    if (this._tcIdx === undefined) this._tcIdx = 0;
    const tc = this.toolConfirmCall;
    const summary = tc ? toolSummary(tc.name, tc.arguments || {}) : '';
    const tcClasses = ['tc-approve', 'tc-deny', 'tc-always'];

    let html = `<div class="tc-summary"><span class="tc-icon">\u26A1</span> ${this._esc(summary)}</div>`;
    for (let i = 0; i < TOOL_CONFIRM_LABELS.length; i++) {
      const cls = `picker-item ${tcClasses[i]}` + (i === this._tcIdx ? ' active' : '');
      const ind = i === this._tcIdx ? '\u276F' : ' ';
      html += `<div class="${cls}" data-idx="${i}">` +
        `<span class="pi-indicator">${ind}</span>` +
        `<span class="pi-label">${this._esc(TOOL_CONFIRM_LABELS[i])}</span></div>`;
    }
    html += '<div class="picker-hint">Tap to select</div>';
    this._pickerOverlay.innerHTML = html;
    this._pickerOverlay.style.display = 'block';
    this.fitAddon.fit();

    this._pickerOverlay.querySelectorAll('.picker-item').forEach(el => {
      el.addEventListener('click', () => {
        this._toolConfirmSelect(parseInt(el.dataset.idx));
      });
    });
  }

  _toolConfirmSelect(idx) {
    const tc = this.toolConfirmCall;
    if (!tc) return;

    const approved = idx !== 1;
    const always = idx === 2;

    this.inToolConfirm = false;
    this.toolConfirmCall = null;
    this._hidePicker();

    const verdict = approved
      ? (always ? `${C.yellow}  \u2713 Always approved${C.reset}` : `${C.green}  \u2713 Approved${C.reset}`)
      : `${C.red}  \u2717 Denied${C.reset}`;
    this._pushLine(verdict);
    this._pushLine('');

    this._sendJson({ type: 'tool_confirm', tool_call_id: tc.id, approved, always });
    this._fullRepaint();
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
  // DOM: User prompt picker
  // -------------------------------------------------------------------------

  _renderUserPrompt() {
    const opts = this.userPromptOptions;
    let html = '';
    for (let i = 0; i < opts.length; i++) {
      const active = i === this.userPromptIdx ? ' active' : '';
      const sel = this.userPromptSelected[i] ? ' selected' : '';
      if (this.userPromptMulti) {
        const check = this.userPromptSelected[i] ? '[x]' : '[ ]';
        html += `<div class="picker-item${active}${sel}" data-idx="${i}">` +
          `<span class="pi-check">${check}</span>` +
          `<span class="pi-label">${this._esc(opts[i])}</span></div>`;
      } else {
        const ind = i === this.userPromptIdx ? '\u276F' : ' ';
        html += `<div class="picker-item${active}" data-idx="${i}">` +
          `<span class="pi-indicator">${ind}</span>` +
          `<span class="pi-label">${this._esc(opts[i])}</span></div>`;
      }
    }
    const hintText = this.userPromptMulti ? 'Tap to toggle, then confirm' : 'Tap to select';
    html += `<div class="picker-hint">${hintText}</div>`;
    if (this.userPromptMulti) {
      html += `<div class="picker-item tc-approve" data-action="confirm">` +
        `<span class="pi-indicator">\u2713</span>` +
        `<span class="pi-label">Confirm selection</span></div>`;
    }
    this._pickerOverlay.innerHTML = html;
    this._pickerOverlay.style.display = 'block';
    this.fitAddon.fit();

    this._pickerOverlay.querySelectorAll('.picker-item').forEach(el => {
      el.addEventListener('click', () => {
        if (el.dataset.action === 'confirm') {
          this._userPromptSelect();
          return;
        }
        const idx = parseInt(el.dataset.idx);
        if (this.userPromptMulti) {
          this.userPromptSelected[idx] = !this.userPromptSelected[idx];
          this.userPromptIdx = idx;
          this._renderUserPrompt();
        } else {
          this.userPromptIdx = idx;
          this._userPromptSelect();
        }
      });
    });
  }

  _userPromptSelect() {
    this.inUserPrompt = false;
    let selected;
    if (this.userPromptMulti) {
      selected = [];
      for (let i = 0; i < this.userPromptSelected.length; i++) {
        if (this.userPromptSelected[i]) selected.push(i);
      }
    } else {
      selected = [this.userPromptIdx];
    }

    this._hidePicker();

    // Show selection in output
    const opts = this.userPromptOptions;
    if (this.userPromptMulti) {
      for (let i = 0; i < opts.length; i++) {
        if (this.userPromptSelected[i]) {
          this._pushLine(`${C.green}  [x] ${C.white}${opts[i]}${C.reset}`);
        }
      }
    } else {
      this._pushLine(`${C.yellow}  \u276F ${C.white}${opts[this.userPromptIdx]}${C.reset}`);
    }
    this._pushLine('');

    // Check if this is a task plan confirmation (reused prompt picker)
    if (this.userPromptId && this.userPromptId.startsWith('__taskplan__')) {
      const planId = this.userPromptId.replace('__taskplan__', '');
      const approved = selected[0] === 0; // 0 = Approve, 1 = Reject
      this._sendJson({ type: 'task_plan_response', plan_id: planId, approved });
    } else {
      this._sendJson({ type: 'user_prompt_response', prompt_id: this.userPromptId, selected });
    }
    this._fullRepaint();
  }

  // -------------------------------------------------------------------------
  // History navigation (reads/writes DOM input)
  // -------------------------------------------------------------------------

  _historyUp() {
    if (this.history.length === 0) return;
    if (this.historyIdx === -1) {
      this.savedInput = this._inputField.value;
      this.historyIdx = this.history.length - 1;
    } else if (this.historyIdx > 0) {
      this.historyIdx--;
    }
    this._inputField.value = this.history[this.historyIdx];
    this._updatePromptLabel();
    this._updateSlashDropdown();
  }

  _historyDown() {
    if (this.historyIdx === -1) return;
    if (this.historyIdx < this.history.length - 1) {
      this.historyIdx++;
      this._inputField.value = this.history[this.historyIdx];
    } else {
      this.historyIdx = -1;
      this._inputField.value = this.savedInput;
    }
    this._updatePromptLabel();
    this._updateSlashDropdown();
  }

  // -------------------------------------------------------------------------
  // Submit (reads DOM input)
  // -------------------------------------------------------------------------

  _submitInputToWarning() {
    const text = this._inputField.value.trim();
    this._inputField.value = '';
    this._updatePromptLabel();
    this._hideSlashDropdown();

    if (!text) return;

    // Show submitted line in output
    const prompt = text.startsWith('/') ? 'cmd-> ' : text.startsWith('!') ? 'shell>' : 'bear> ';
    this._pushLine(`  ${C.bold}${C.white}${prompt}${C.reset}${C.white}${text}${C.reset}`);

    if (this.history.length === 0 || this.history[this.history.length - 1] !== text) {
      this.history.push(text);
    }
    this.historyIdx = -1;
    this.savedInput = '';

    // Buffer the text and start the warning countdown
    this._interruptPendingText = text;
    this._interruptWarningStart = Date.now();

    // Tick the warning bar every 100ms, auto-dismiss at 0
    this._interruptWarningTimer = setInterval(() => {
      if (this._interruptWarningRemainingMs() <= 0) {
        this._dismissInterruptWarning();
      }
      this._drawStatusBar();
    }, 100);

    this._drawStatusBar();
    this._fullRepaint();
  }

  _submitInput() {
    const text = this._inputField.value.trim();
    this._inputField.value = '';
    this._updatePromptLabel();
    this._hideSlashDropdown();

    if (!text) return;

    // Show submitted line in output
    const prompt = text.startsWith('/') ? 'cmd-> ' : text.startsWith('!') ? 'shell>' : 'bear> ';
    this._pushLine(`  ${C.bold}${C.white}${prompt}${C.reset}${C.white}${text}${C.reset}`);

    if (this.history.length === 0 || this.history[this.history.length - 1] !== text) {
      this.history.push(text);
    }
    this.historyIdx = -1;
    this.savedInput = '';

    // Slash commands
    if (text === '/help') {
      this._showHelp();
      this._fullRepaint();
      return;
    }

    // /allowed is handled server-side as a slash command

    if (text === '/end') {
      if (this._isConnected()) this._sendJson({ type: 'session_end' });
      this._pushLine(`${C.gray}  Session ended. Reconnecting…${C.reset}`);
      this._pushLine('');
      this._fullRepaint();
      // Reset reconnect state — intentional reconnect to lobby
      this._lastSessionId = null;
      this._autoRejoining = false;
      this._reconnectAttempt = 0;
      this._nonRecoverable = false;
      this._wasConnected = false;
      this._connectRelay();
      return;
    }

    if (text === '/exit') {
      this._pushLine(`${C.gray}  Disconnecting. Session preserved. Reconnecting…${C.reset}`);
      this._pushLine('');
      this._fullRepaint();
      // Reset reconnect state — intentional reconnect to lobby
      this._lastSessionId = null;
      this._autoRejoining = false;
      this._reconnectAttempt = 0;
      this._nonRecoverable = false;
      this._wasConnected = false;
      this._connectRelay();
      return;
    }

    if (!this._isConnected()) {
      this._pushLine(`${C.red}  Not connected. Use /exit to pick a session.${C.reset}`);
      this._fullRepaint();
      return;
    }

    if (text === '/ps') {
      this._sendJson({ type: 'process_list' });
      this._fullRepaint();
      return;
    }

    const killMatch = text.match(/^\/kill\s+(\d+)$/);
    if (killMatch) {
      this._sendJson({ type: 'process_kill', pid: parseInt(killMatch[1]) });
      this._fullRepaint();
      return;
    }

    const sendMatch = text.match(/^\/send\s+(\d+)\s+(.+)$/);
    if (sendMatch) {
      this._sendJson({ type: 'process_input', pid: parseInt(sendMatch[1]), text: sendMatch[2] });
      this._fullRepaint();
      return;
    }

    const sessionNameMatch = text.match(/^\/session\s+name\s+(.+)$/);
    if (sessionNameMatch) {
      const name = sessionNameMatch[1].trim();
      if (!name) {
        this._pushLine(`${C.red}  Usage: /session name <session name>${C.reset}`);
      } else {
        this._sendJson({ type: 'session_rename', name });
      }
      this._fullRepaint();
      return;
    }

    const sessionWorkdirMatch = text.match(/^\/session\s+workdir\s+(.+)$/);
    if (sessionWorkdirMatch) {
      const path = sessionWorkdirMatch[1].trim();
      if (!path) {
        this._pushLine(`${C.red}  Usage: /session workdir <path>${C.reset}`);
      } else {
        this._sendJson({ type: 'session_workdir', path });
      }
      this._fullRepaint();
      return;
    }

    if (text.startsWith('/session')) {
      this._pushLine(`${C.red}  Usage: /session name <session name> OR /session workdir <path>${C.reset}`);
      this._fullRepaint();
      return;
    }

    // Shell execution via ! prefix
    if (text.startsWith('!')) {
      const cmd = text.slice(1).trim();
      if (!cmd) {
        this._pushLine(`${C.red}  Usage: !<command>${C.reset}`);
        this._fullRepaint();
        return;
      }
      this._awaitingInputEcho = true;
      this._sendJson({ type: 'shell_exec', command: cmd });
      this._fullRepaint();
      return;
    }

    // Regular chat
    this._awaitingInputEcho = true;
    this._sendJson({ type: 'input', text: text });
    this._fullRepaint();
  }

  // -------------------------------------------------------------------------
  // Tool output rendering
  // -------------------------------------------------------------------------

  _renderToolOutput(toolName, toolArgs, output) {
    const MAX_LINES = 20;

    switch (toolName) {
      case 'read_file': {
        const path = toolArgs.path || '?';
        if (output.startsWith('Error')) {
          this._pushLine(`${C.red}  ✗ ${output}${C.reset}`);
        } else {
          const lineCount = output.split('\n').length;
          this._pushLine(`${C.green}  ✓ Read ${path} (${lineCount} lines)${C.reset}`);
        }
        break;
      }
      case 'write_file':
      case 'edit_file':
      case 'patch_file': {
        const isErr = output.startsWith('Error') || output.startsWith('Patch failed');
        const blankIdx = output.indexOf('\n\n');
        const status = blankIdx >= 0 ? output.substring(0, blankIdx) : output;
        const diff = blankIdx >= 0 ? output.substring(blankIdx + 2).trimEnd() : null;
        const color = isErr ? C.red : C.green;
        const icon = isErr ? '✗' : '✓';
        this._pushLine(`${color}  ${icon} ${status}${C.reset}`);
        if (diff) this._writeDiffToBuffer(diff, MAX_LINES * 2);
        break;
      }
      case 'run_command':
        this._writeTruncatedToBuffer(output, MAX_LINES);
        break;
      case 'list_files': {
        const count = output.split('\n').filter(l => l.length > 0).length;
        this._pushLine(`${C.green}  ✓ ${count} entries${C.reset}`);
        this._writeTruncatedToBuffer(output, MAX_LINES);
        break;
      }
      case 'search_text': {
        if (output === 'No matches found.') {
          this._pushLine(`${C.gray}  │ ${output}${C.reset}`);
        } else {
          const count = output.split('\n').filter(l => l.length > 0 && !l.startsWith('[')).length;
          this._pushLine(`${C.green}  ✓ ${count} matches${C.reset}`);
          this._writeTruncatedToBuffer(output, MAX_LINES);
        }
        break;
      }
      case 'undo': {
        const isNoop = output.startsWith('Error') || output === 'Nothing to undo.';
        const color = isNoop ? C.gray : C.green;
        const icon = isNoop ? '│' : '✓';
        this._pushLine(`${color}  ${icon} ${output}${C.reset}`);
        break;
      }
      case 'user_prompt_options':
        this._pushLine(`${C.cyan}  │ ${output}${C.reset}`);
        break;
      default:
        this._writeTruncatedToBuffer(output, MAX_LINES);
        break;
    }
  }

  _writeDiffToBuffer(diff, maxLines) {
    const lines = diff.split('\n');
    const total = lines.length;
    const render = (line) => {
      if (line.startsWith('+++') || line.startsWith('---')) {
        return `${C.bold}${C.white}    ${line}${C.reset}`;
      } else if (line.startsWith('@@')) {
        return `${C.cyan}    ${line}${C.reset}`;
      } else if (line.startsWith('+')) {
        return `${C.green}    ${line}${C.reset}`;
      } else if (line.startsWith('-')) {
        return `${C.red}    ${line}${C.reset}`;
      } else {
        return `${C.gray}    ${line}${C.reset}`;
      }
    };
    if (total <= maxLines) {
      for (const line of lines) this._pushLine(render(line));
    } else {
      const head = Math.floor(maxLines / 2);
      const tail = maxLines - head;
      for (let i = 0; i < head; i++) this._pushLine(render(lines[i]));
      this._pushLine(`${C.gray}    … (${total - head - tail} lines hidden) …${C.reset}`);
      for (let i = total - tail; i < total; i++) this._pushLine(render(lines[i]));
    }
  }

  _writeTruncatedToBuffer(output, maxLines) {
    const lines = output.split('\n');
    const total = lines.length;
    if (total <= maxLines) {
      for (const line of lines) this._pushLine(`${C.gray}  │ ${line}${C.reset}`);
      return;
    }
    const head = Math.floor(maxLines / 2);
    const tail = maxLines - head;
    for (let i = 0; i < head; i++) this._pushLine(`${C.gray}  │ ${lines[i]}${C.reset}`);
    this._pushLine(`${C.dim}${C.gray}  │   … (${total - head - tail} lines hidden) …${C.reset}`);
    for (let i = total - tail; i < total; i++) this._pushLine(`${C.gray}  │ ${lines[i]}${C.reset}`);
  }

  // -------------------------------------------------------------------------
  // Help
  // -------------------------------------------------------------------------

  _showHelp() {
    const commandLines = this.slashCommands.length > 0
      ? this.slashCommands.map(({ cmd, desc }) => {
        const padded = cmd.padEnd(20, ' ');
        return `${C.gray}    ${padded}${C.white}${desc}${C.reset}`;
      })
      : [`${C.gray}    (commands not loaded yet)${C.reset}`];

    const lines = [
      '',
      `${C.bold}${C.white}  Commands:${C.reset}`,
      ...commandLines,
      '',
      `${C.bold}${C.white}  Tool confirmations:  ${C.gray}(interactive picker)${C.reset}`,
      `${C.green}    Approve          ${C.white}Allow this tool call${C.reset}`,
      `${C.red}    Deny             ${C.white}Reject this tool call${C.reset}`,
      `${C.yellow}    Always approve   ${C.white}Auto-approve this command for the session${C.reset}`,
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
    for (const l of lines) this._pushLine(l);
  }

  // -------------------------------------------------------------------------
  // Audio alert
  // -------------------------------------------------------------------------

  _playAlert() {
    try {
      if (!this._audioCtx) {
        this._audioCtx = new (window.AudioContext || window.webkitAudioContext)();
      }
      const ctx = this._audioCtx;
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      osc.connect(gain);
      gain.connect(ctx.destination);
      osc.type = 'sine';
      osc.frequency.setValueAtTime(880, ctx.currentTime);
      gain.gain.setValueAtTime(0.3, ctx.currentTime);
      gain.gain.exponentialRampToValueAtTime(0.001, ctx.currentTime + 0.15);
      osc.start(ctx.currentTime);
      osc.stop(ctx.currentTime + 0.15);
    } catch (_) {}
  }

  // -------------------------------------------------------------------------
  // Transport
  // -------------------------------------------------------------------------

  _startHeartbeat() {
    this._stopHeartbeat();
    this._lastPongAt = Date.now();
    this._heartbeatTimer = setInterval(() => {
      if (!this._isConnected()) {
        this._stopHeartbeat();
        return;
      }
      // Check if we got a pong recently
      if (Date.now() - this._lastPongAt > 20000) {
        console.warn('[bear] Heartbeat timeout — no pong for 20s');
        this._stopHeartbeat();
        this._cleanup();
        this._scheduleReconnect();
        return;
      }
      this._sendJson({ type: 'ping' });
    }, 5000);
  }

  _stopHeartbeat() {
    if (this._heartbeatTimer) {
      clearInterval(this._heartbeatTimer);
      this._heartbeatTimer = null;
    }
  }

  _sendJson(obj) {
    const payload = JSON.stringify(obj);
    if (this.dc && this.dc.readyState === 'open') {
      this.dc.send(payload);
    } else {
      console.warn('bear: _sendJson called but DataChannel is not open', obj);
    }
  }
}
