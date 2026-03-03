// ---------------------------------------------------------------------------
// Bear Browser Client — OpenCode-style TUI powered by xterm.js
// ---------------------------------------------------------------------------
// version 0.1.12
// Relay configuration: these globals must be set by the hosting page.
// bear.js communicates exclusively via the public server, which proxies
// all signaling (offer, answer, ICE) to the relay on behalf of the browser.
const RELAY_ROOM = (typeof window !== 'undefined' && window.BEAR_ROOM_ID) ? window.BEAR_ROOM_ID : null;
const PUBLIC_URL = (typeof window !== 'undefined' && window.BEAR_PUBLIC_URL != null) ? window.BEAR_PUBLIC_URL : '';
const HOME_URL = (typeof window !== 'undefined' && window.BEAR_HOME) ? window.BEAR_HOME : '/dashboard';

// ICE servers: STUN defaults + optional TURN servers injected by the hosting page
// The public server sets window.BEAR_ICE_SERVERS (array of {urls, username, credential})
// by fetching credentials from the relay's /internal/turn-credentials endpoint.
const ICE_SERVERS = [
  { urls: 'stun:stun.l.google.com:19302' },
  { urls: 'stun:stun1.l.google.com:19302' },
  ...((typeof window !== 'undefined' && Array.isArray(window.BEAR_ICE_SERVERS)) ? window.BEAR_ICE_SERVERS : []),
];

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

// Box drawing
const BOX = { tl: '┌', tr: '┐', bl: '└', br: '┘', h: '─', v: '│' };

// Tool confirmation picker
const TOOL_CONFIRM_LABELS = ['Approve', 'Deny', 'Always approve for session'];
const TOOL_CONFIRM_COLORS = [C.green, C.red, C.yellow];

// Spinner frames
const SPINNER = ['·····', '●····', '·●···', '··●··', '···●·', '····●', '·····'];

// Layout
const INPUT_BOX_H = 3; // top border + input + bottom border
const STATUS_BAR_H = 1;
const BOTTOM_H = INPUT_BOX_H + STATUS_BAR_H;


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

    // Input state
    this.inputBuf = '';
    this.cursorPos = 0;
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

    // Slash command dropdown state
    this._dropdownLines = 0;
    this._dropdownIdx = -1;
    this.slashCommands = [];

    // Echo suppression: skip the next UserInput echo from the server
    this._awaitingInputEcho = false;

    // Heartbeat: detect stale connections
    this._heartbeatTimer = null;
    this._lastPongAt = 0;

    // Last tool tracking
    this._lastToolName = '';
    this._lastToolArgs = {};

    // Tool confirmation picker state
    this.inToolConfirm = false;
    this.toolConfirmCall = null;
    this.toolConfirmIdx = 0;
    this.toolConfirmRendered = false;
    this._lastExtractedCommands = [];

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

    // Active subagent tracking
    this._activeSubagents = new Set();

    // Interrupt warning state (double-Enter to interrupt LLM)
    this._interruptPendingText = null;
    this._interruptWarningStart = null;
    this._interruptWarningTimer = null;

    // Screen dimensions
    this._cols = term.cols || 80;
    this._rows = term.rows || 24;

    this._bindTerminal();
    this._bindResize();
  }

  // -------------------------------------------------------------------------
  // Screen geometry
  // -------------------------------------------------------------------------

  _outputAreaHeight() {
    return Math.max(1, this._rows - BOTTOM_H);
  }

  _inputRow() {
    return this._rows - BOTTOM_H;
  }

  _statusRow() {
    return this._rows - 1;
  }

  // Compute the scroll offset for the input buffer so the cursor stays visible.
  _inputScrollStart() {
    const innerW = Math.max(0, this._cols - 4);
    const textSpace = Math.max(0, innerW - 6); // promptLen = 6
    if (this.inputBuf.length <= textSpace || textSpace <= 0) return 0;
    let start = Math.max(0, this.cursorPos - textSpace + 1);
    return Math.min(start, this.inputBuf.length - textSpace);
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
    this.term.write('\x1b[?25l'); // hide cursor
    this._drawOutputArea();
    this._drawInputBox();
    this._drawStatusBar();
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

  _drawInputBox() {
    this.term.write('\x1b[?25l'); // hide cursor during redraw
    const row = this._inputRow() + 1; // 1-indexed
    const w = this._cols;
    const borderW = Math.max(0, w - 2);

    // Top border
    this.term.write(`\x1b[${row};1H\x1b[2K`);
    this.term.write(`${C.gray}${BOX.tl}${BOX.h.repeat(borderW)}${BOX.tr}${C.reset}`);

    // Input line
    this.term.write(`\x1b[${row + 1};1H\x1b[2K`);
    const isSlash = this.inputBuf.startsWith('/');
    const isShell = this.inputBuf.startsWith('!');
    const prompt = isSlash ? 'cmd-> ' : isShell ? 'shell>' : 'bear> ';
    const promptColor = isSlash ? C.yellow : isShell ? C.magenta : C.cyan;
    const innerW = Math.max(0, w - 4);
    const promptLen = 6;
    const textSpace = Math.max(0, innerW - promptLen);
    const scrollStart = this._inputScrollStart();
    const displayText = this.inputBuf.length > textSpace
      ? this.inputBuf.slice(scrollStart, scrollStart + textSpace)
      : this.inputBuf;
    const padding = Math.max(0, innerW - promptLen - displayText.length);

    this.term.write(
      `${C.gray}${BOX.v} ${C.reset}` +
      `${promptColor}${C.bold}${prompt}${C.reset}` +
      `${displayText}` +
      `${' '.repeat(padding)}` +
      `${C.gray} ${BOX.v}${C.reset}`
    );

    // Bottom border
    this.term.write(`\x1b[${row + 2};1H\x1b[2K`);
    this.term.write(`${C.gray}${BOX.bl}${BOX.h.repeat(borderW)}${BOX.br}${C.reset}`);

    // Position cursor (relative to scroll window)
    const cursorCol = 3 + promptLen + (this.cursorPos - scrollStart);

    // Dropdown above input box
    if (isSlash) {
      const matches = matchingSlashCommands(this.inputBuf, this.slashCommands);
      if (matches.length > 0) {
        if (this._dropdownIdx >= matches.length) {
          this._dropdownIdx = matches.length - 1;
        }
        const ddStart = row - matches.length; // rows above input box
        for (let i = 0; i < matches.length; i++) {
          const r = ddStart + i;
          if (r < 1) continue;
          this.term.write(`\x1b[${r};1H\x1b[2K`);
          const { cmd, desc } = matches[i];
          if (i === this._dropdownIdx) {
            this.term.write(`${C.yellow}  ❯ ${C.white}${cmd}${C.gray}  ${desc}${C.reset}`);
          } else {
            this.term.write(`${C.gray}    ${C.yellow}${cmd}${C.gray}  ${desc}${C.reset}`);
          }
        }
        this._dropdownLines = matches.length;
      } else {
        this._dropdownLines = 0;
      }
    } else {
      this._dropdownLines = 0;
    }

    // Position cursor and show it only after all rendering is complete
    this.term.write(`\x1b[${row + 1};${cursorCol}H\x1b[?25h`);
  }

  _drawStatusBar() {
    this.term.write('\x1b[?25l'); // hide cursor during redraw
    const row = this._statusRow() + 1; // 1-indexed
    const w = this._cols;

    this.term.write(`\x1b[${row};1H\x1b[2K`);

    const remainingMs = this._interruptWarningRemainingMs();

    if (remainingMs > 0) {
      const warnText = 'LLM is busy \u2014 press Enter again to interrupt';
      const barMax = warnText.length;
      const barLen = Math.min(barMax, Math.floor(barMax * remainingMs / 6000));

      this.term.write(`\x1b[${row};2H${C.yellow}${warnText}${C.reset}`);
      if (barLen > 0) {
        const barStart = Math.max(1, w - barLen);
        this.term.write(`\x1b[${row};${barStart}H\x1b[38;5;136m${'▁'.repeat(barLen)}${C.reset}`);
      }
    } else {
      const spinner = this._streaming
        ? SPINNER[this._spinnerFrame % SPINNER.length]
        : '·····';
      const session = this._sessionName || 'bear';

      const subagentInfo = this._activeSubagents.size > 0
        ? `  🔍${this._activeSubagents.size}`
        : '';
      const left = `${spinner}  ${session}${subagentInfo}`;
      const right = '↑↓ history  pgup/pgdn scroll  ctrl+c go to home';

      const gap = Math.max(1, w - left.length - right.length - 2);

      this.term.write(`${C.gray} ${left}${' '.repeat(gap)}${right} ${C.reset}`);
    }

    // Restore cursor to input box
    const inputRow = this._inputRow() + 2; // 1-indexed, +1 for input line within box
    const scrollStart = this._inputScrollStart();
    const cursorCol = 3 + 6 + (this.cursorPos - scrollStart);
    this.term.write(`\x1b[${inputRow};${cursorCol}H\x1b[?25h`);
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
    this._pushLine(`${C.gray}    Type /help for commands${C.reset}`);
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
    this._pushLine('');

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

    // Remove previous picker lines
    if (this.pickerRendered) {
      this._popLines(items.length + 1); // items + hint
    }
    this.pickerRendered = true;

    for (let i = 0; i < items.length; i++) {
      const selected = i === this.pickerIdx;
      const prefix = selected ? `${C.bold}${C.blue}  ❯ ` : `${C.gray}    `;
      const labelColor = selected ? C.white : C.gray;
      const detailStr = items[i].detail ? `  ${C.dim}${C.gray}${items[i].detail}${C.reset}` : '';
      this._pushLine(`${prefix}${labelColor}${items[i].label}${C.reset}${detailStr}`);
    }
    this._pushLine(`${C.gray}  ↑/↓ navigate, Enter select${C.reset}`);
    this._fullRepaint();
  }

  _pickerSelect() {
    this.inSessionPicker = false;
    this._pushLine('');

    if (this.pickerIdx === 0) {
      this._pushLine(`${C.gray}  Creating new session…${C.reset}`);
      this._fullRepaint();
      this._sendJson({ type: 'session_create', cwd: null });
    } else {
      const session = this.pickerSessions[this.pickerIdx - 1];
      this._pushLine(`${C.gray}  Connecting to session ${session.id.substring(0, 8)}…${C.reset}`);
      this._fullRepaint();
      this._sendJson({ type: 'session_select', session_id: session.id });
    }
  }

  // -------------------------------------------------------------------------
  // WebRTC DataChannel connection
  // -------------------------------------------------------------------------

  _connectRelay() {
    this.inToolConfirm = false;
    this.toolConfirmCall = null;
    this.toolConfirmIdx = 0;
    this.toolConfirmRendered = false;
    this.inUserPrompt = false;
    this._activeSubagents = new Set();

    this._cleanup();

    this._pushLine(`${C.gray}  Connecting via WebRTC…${C.reset}`);
    this._fullRepaint();

    this.pc = new RTCPeerConnection({ iceServers: ICE_SERVERS });
    this.dc = this.pc.createDataChannel('bear', { ordered: true });

    this.dc.onopen = () => {
      this._startHeartbeat();
      // DataChannel is open — lobby session list request is deferred
      // until the server sends slash_commands (its "lobby ready" signal),
      // ensuring the server's on_message handler is registered first.
      this._lobbyPending = true;
    };

    this.dc.onclose = () => {
      this._pushLine(`${C.gray}  Disconnected.${C.reset}`);
      this._stopSpinner();
      this._fullRepaint();
    };

    this.dc.onerror = (e) => {
      this._pushLine(`${C.red}  DataChannel error: ${e.error?.message || 'unknown'}${C.reset}`);
      this._fullRepaint();
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
      if (this.pc.connectionState === 'failed' || this.pc.connectionState === 'disconnected') {
        this._pushLine(`${C.red}  Connection lost.${C.reset}`);
        this._stopSpinner();
        this._fullRepaint();
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
        this._pushLine(`${C.red}  Relay signaling failed: ${offerRes.status}${C.reset}`);
        this._fullRepaint();
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
      this._pushLine(`${C.red}  Relay signaling timeout: no answer received${C.reset}`);
      this._fullRepaint();
    } catch (e) {
      this._pushLine(`${C.red}  Relay signaling error: ${e.message}${C.reset}`);
      this._fullRepaint();
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
  // Server message dispatch
  // -------------------------------------------------------------------------

  _handleServerMessage(msg) {
    switch (msg.type) {
      case 'slash_commands':
        this.slashCommands = Array.isArray(msg.commands) ? msg.commands : [];
        this._drawInputBox();
        // slash_commands is the server's "lobby ready" signal — now safe
        // to request the session list over the DataChannel.
        if (this._lobbyPending) {
          this._lobbyPending = false;
          this._showSessionPicker();
        }
        break;

      case 'session_list_result': {
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
        this._pushLine('');
        this._pushLine(`${C.cyan}  Connected to session ${C.bold}${msg.session.id}${C.reset}`);
        this._pushLine(`${C.gray}  Working directory: ${msg.session.cwd}${C.reset}`);
        this._pushLine(`${C.gray}  Type /help for commands${C.reset}`);
        this._pushLine('');
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
        this.toolConfirmIdx = 0;
        this.toolConfirmRendered = false;
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
          // Remove picker lines (summary + options + hint)
          this._popLines(this._toolConfirmPickerLines());
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
          // Remove prompt picker lines
          this._popLines(this.userPromptOptions.length + 1); // options + hint
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
        this.userPromptRendered = false;
        this._pushLine(`${C.bold}${C.cyan}  ${msg.question}${C.reset}`);
        this._pushLine('');
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
        this.userPromptRendered = false;
        this._pushLine(`${C.bold}${C.cyan}  Execute this plan?${C.reset}`);
        this._pushLine('');
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
  // Terminal input binding
  // -------------------------------------------------------------------------

  _bindTerminal() {
    // Mouse wheel / trackpad scroll → viewport scroll.
    // xterm.js attaches its own wheel handler on an internal child
    // element (.xterm-viewport) and translates wheel events into
    // \x1b[A / \x1b[B arrow-key sequences via onData, which would
    // trigger history navigation.  We intercept on the *document* at
    // the capture phase so we fire before any element-level handlers,
    // then stopImmediatePropagation to prevent xterm.js from seeing it.
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

    this.term.onData((data) => {
      if (this.inSessionPicker) { this._handlePickerKey(data); return; }
      if (this.inToolConfirm) { this._handleToolConfirmKey(data); return; }
      if (this.inUserPrompt) { this._handleUserPromptKey(data); return; }

      for (let i = 0; i < data.length; i++) {
        const ch = data[i];
        const code = ch.charCodeAt(0);

        // ESC sequence
        if (ch === '\x1b' && data[i + 1] === '[') {
          const arrow = data[i + 2];
          i += 2;

          if (this._dropdownActive()) {
            if (arrow === 'A') { this._dropdownUp(); this._drawInputBox(); continue; }
            if (arrow === 'B') { this._dropdownDown(); this._drawInputBox(); continue; }
            this._dropdownIdx = -1;
          }

          if (arrow === 'A') { this._historyUp(); continue; }
          if (arrow === 'B') { this._historyDown(); continue; }
          if (arrow === 'C') { this._cursorRight(); continue; }
          if (arrow === 'D') { this._cursorLeft(); continue; }

          // Page up/down: \x1b[5~ and \x1b[6~
          if (arrow === '5' && data[i + 1] === '~') { i++; this._scrollUp(5); this._fullRepaint(); continue; }
          if (arrow === '6' && data[i + 1] === '~') { i++; this._scrollDown(5); this._fullRepaint(); continue; }
          continue;
        }

        // Bare Esc
        if (code === 27) {
          if (this._dropdownActive()) {
            this.inputBuf = '';
            this.cursorPos = 0;
            this._dropdownIdx = -1;
            this._drawInputBox();
          }
          continue;
        }

        // Tab
        if (code === 9) {
          if (this._dropdownActive()) {
            this._acceptDropdown();
            this._drawInputBox();
          }
          continue;
        }

        // Enter
        if (ch === '\r' || ch === '\n') {
          if (this._dropdownActive() && this._dropdownIdx >= 0) {
            this._acceptDropdown();
            this._drawInputBox();
            continue;
          }
          // Double-Enter interrupt: if warning is active, confirm the interrupt
          if (this._interruptPendingText !== null && this._interruptWarningRemainingMs() > 0) {
            const text = this._interruptPendingText;
            this._dismissInterruptWarning();
            this._awaitingInputEcho = true;
            this._sendJson({ type: 'input', text });
            this._fullRepaint();
            continue;
          }
          // If LLM is busy and user typed non-empty text, show warning instead of sending
          if (this._streaming && this.inputBuf.trim()) {
            this._submitInputToWarning();
            continue;
          }
          // Normal submit
          this._dismissInterruptWarning();
          this._submitInput();
          continue;
        }

        // Backspace
        if (code === 127 || code === 8) {
          this._dropdownIdx = -1;
          if (this._interruptPendingText !== null) this._dismissInterruptWarning();
          this._backspace();
          continue;
        }

        // Ctrl+C — navigate to home
        if (code === 3) {
          window.location.href = HOME_URL;
          continue;
        }

        // Ctrl+D
        if (code === 4) continue;

        // Ctrl+U
        if (code === 21) {
          this.inputBuf = '';
          this.cursorPos = 0;
          this._drawInputBox();
          continue;
        }

        // Printable
        if (code >= 32) {
          this._dropdownIdx = -1;
          if (this._interruptPendingText !== null) this._dismissInterruptWarning();
          this.inputBuf = this.inputBuf.slice(0, this.cursorPos) + ch + this.inputBuf.slice(this.cursorPos);
          this.cursorPos++;
          this._drawInputBox();
        }
      }
    });
  }

  // -------------------------------------------------------------------------
  // Picker key handling
  // -------------------------------------------------------------------------

  _handlePickerKey(data) {
    const totalItems = this.pickerSessions.length + 1;
    if (data === '\x1b[A') {
      if (this.pickerIdx > 0) { this.pickerIdx--; this._renderPicker(); }
    } else if (data === '\x1b[B') {
      if (this.pickerIdx < totalItems - 1) { this.pickerIdx++; this._renderPicker(); }
    } else if (data === '\r' || data === '\n') {
      this._pickerSelect();
    }
  }

  // -------------------------------------------------------------------------
  // User prompt
  // -------------------------------------------------------------------------

  _handleUserPromptKey(data) {
    const total = this.userPromptOptions.length;
    if (data === '\x1b[A') {
      if (this.userPromptIdx > 0) { this.userPromptIdx--; this._renderUserPrompt(); }
    } else if (data === '\x1b[B') {
      if (this.userPromptIdx < total - 1) { this.userPromptIdx++; this._renderUserPrompt(); }
    } else if (data === ' ' && this.userPromptMulti) {
      this.userPromptSelected[this.userPromptIdx] = !this.userPromptSelected[this.userPromptIdx];
      this._renderUserPrompt();
    } else if (data === '\r' || data === '\n') {
      this._userPromptSelect();
    }
  }

  _renderUserPrompt() {
    const opts = this.userPromptOptions;
    const removeCount = opts.length + 1;

    if (this.userPromptRendered) {
      this._popLines(removeCount);
    }
    this.userPromptRendered = true;

    for (let i = 0; i < opts.length; i++) {
      const focused = i === this.userPromptIdx;
      if (this.userPromptMulti) {
        const check = this.userPromptSelected[i] ? '[x]' : '[ ]';
        if (focused) {
          this._pushLine(`${C.bold}${C.yellow}  ${check} ${C.white}${opts[i]}${C.reset}`);
        } else {
          this._pushLine(`${C.gray}  ${check} ${opts[i]}${C.reset}`);
        }
      } else {
        if (focused) {
          this._pushLine(`${C.bold}${C.blue}  ❯ ${C.white}${opts[i]}${C.reset}`);
        } else {
          this._pushLine(`${C.gray}    ${opts[i]}${C.reset}`);
        }
      }
    }

    const hint = this.userPromptMulti
      ? `${C.gray}  ↑/↓ navigate, Space toggle, Enter confirm${C.reset}`
      : `${C.gray}  ↑/↓ navigate, Enter select${C.reset}`;
    this._pushLine(hint);
    this._fullRepaint();
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

    // Replace picker lines with selection
    const opts = this.userPromptOptions;
    this._popLines(opts.length + 1);

    if (this.userPromptMulti) {
      for (let i = 0; i < opts.length; i++) {
        if (this.userPromptSelected[i]) {
          this._pushLine(`${C.green}  [x] ${C.white}${opts[i]}${C.reset}`);
        }
      }
    } else {
      this._pushLine(`${C.yellow}  ❯ ${C.white}${opts[this.userPromptIdx]}${C.reset}`);
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
  // Tool confirmation picker
  // -------------------------------------------------------------------------

  _handleToolConfirmKey(data) {
    if (data === '\x1b[A') {
      if (this.toolConfirmIdx > 0) { this.toolConfirmIdx--; this._renderToolConfirm(); }
    } else if (data === '\x1b[B') {
      if (this.toolConfirmIdx < TOOL_CONFIRM_LABELS.length - 1) { this.toolConfirmIdx++; this._renderToolConfirm(); }
    } else if (data === '\r' || data === '\n') {
      this._toolConfirmSelect();
    }
  }

  _toolConfirmPickerLines() {
    return TOOL_CONFIRM_LABELS.length + 2; // summary + options + hint
  }

  _renderToolConfirm() {
    if (this.toolConfirmRendered) {
      this._popLines(this._toolConfirmPickerLines());
    }
    this.toolConfirmRendered = true;

    // Summary line so the command is always visible in the picker area
    const tc = this.toolConfirmCall;
    const summary = tc ? toolSummary(tc.name, tc.arguments || {}) : '';
    this._pushLine(`${C.magenta}  ⚡ ${C.white}${summary}${C.reset}`);

    for (let i = 0; i < TOOL_CONFIRM_LABELS.length; i++) {
      const focused = i === this.toolConfirmIdx;
      if (focused) {
        this._pushLine(`${C.yellow}  ❯ ${TOOL_CONFIRM_COLORS[i]}${TOOL_CONFIRM_LABELS[i]}${C.reset}`);
      } else {
        this._pushLine(`${C.gray}    ${TOOL_CONFIRM_LABELS[i]}${C.reset}`);
      }
    }
    this._pushLine(`${C.gray}  ↑/↓ navigate, Enter select${C.reset}`);
    this._fullRepaint();
  }

  _toolConfirmSelect() {
    const tc = this.toolConfirmCall;
    if (!tc) return;

    const cmds = this._lastExtractedCommands || [this._extractBaseCommand(tc)];
    const idx = this.toolConfirmIdx;
    const approved = idx !== 1;

    this.inToolConfirm = false;
    this.toolConfirmCall = null;

    // Replace picker lines with result
    this._popLines(this._toolConfirmPickerLines());

    const always = idx === 2;

    const verdict = approved
      ? (always ? `${C.yellow}  ✓ Always approved${C.reset}` : `${C.green}  ✓ Approved${C.reset}`)
      : `${C.red}  ✗ Denied${C.reset}`;
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
  // Dropdown
  // -------------------------------------------------------------------------

  _dropdownActive() {
    return this._dropdownLines > 0 && this.inputBuf.startsWith('/');
  }

  _dropdownUp() {
    const matches = matchingSlashCommands(this.inputBuf, this.slashCommands);
    if (matches.length === 0) return;
    this._dropdownIdx = this._dropdownIdx <= 0 ? matches.length - 1 : this._dropdownIdx - 1;
  }

  _dropdownDown() {
    const matches = matchingSlashCommands(this.inputBuf, this.slashCommands);
    if (matches.length === 0) return;
    this._dropdownIdx = (this._dropdownIdx < 0 || this._dropdownIdx >= matches.length - 1) ? 0 : this._dropdownIdx + 1;
  }

  _acceptDropdown() {
    const matches = matchingSlashCommands(this.inputBuf, this.slashCommands);
    const idx = this._dropdownIdx >= 0 ? this._dropdownIdx : 0;
    if (idx < matches.length) {
      this.inputBuf = matches[idx].cmd + ' ';
      this.cursorPos = this.inputBuf.length;
    }
    this._dropdownIdx = -1;
    this._dropdownLines = 0;
  }

  // -------------------------------------------------------------------------
  // Input editing
  // -------------------------------------------------------------------------

  _backspace() {
    if (this.cursorPos > 0) {
      this.inputBuf = this.inputBuf.slice(0, this.cursorPos - 1) + this.inputBuf.slice(this.cursorPos);
      this.cursorPos--;
      this._drawInputBox();
    }
  }

  _cursorLeft() {
    if (this.cursorPos > 0) {
      this.cursorPos--;
      this._drawInputBox();
    }
  }

  _cursorRight() {
    if (this.cursorPos < this.inputBuf.length) {
      this.cursorPos++;
      this._drawInputBox();
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
    this._drawInputBox();
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
    this._drawInputBox();
  }

  // -------------------------------------------------------------------------
  // Submit
  // -------------------------------------------------------------------------

  _submitInputToWarning() {
    this._dropdownIdx = -1;
    this._dropdownLines = 0;
    const text = this.inputBuf.trim();
    this.inputBuf = '';
    this.cursorPos = 0;

    if (!text) { this._drawInputBox(); return; }

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

    this._drawInputBox();
    this._drawStatusBar();
  }

  _submitInput() {
    this._dropdownIdx = -1;
    this._dropdownLines = 0;
    const text = this.inputBuf.trim();
    this.inputBuf = '';
    this.cursorPos = 0;

    if (!text) {
      this._drawInputBox();
      return;
    }

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
      // Re-establish WebRTC connection to get back to lobby
      this._connectRelay();
      return;
    }

    if (text === '/exit') {
      this._pushLine(`${C.gray}  Disconnecting. Session preserved. Reconnecting…${C.reset}`);
      this._pushLine('');
      this._fullRepaint();
      // Re-establish WebRTC connection to get back to lobby
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
        this._pushLine(`${C.red}  Connection lost (no heartbeat response).${C.reset}`);
        this._stopHeartbeat();
        this._cleanup();
        this._fullRepaint();
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
