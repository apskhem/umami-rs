// Translates JavaScript regular expressions into Rust `regex` / `fancy-regex` syntax,
// preserving JS semantics where the two differ for the patterns we port:
//
// - `\d`, `\w` (and negations) are ASCII-only in JS but Unicode-aware in Rust.
// - JS treats an unmatched `{`/`}` and a `[` inside a class as literals; Rust does not.
// - `-` directly after a class escape (`[\w-.]`) is a literal in JS.
// - `&`, `~` and `--` are class set operators in Rust.
// - Quantified lookahead (allowed in JS) is reduced to its meaning.
// - Backreferences (`\1`) are kept; they need fancy-regex.
// - Negative lookbehind with alternation is split into one lookbehind per branch,
//   since fancy-regex only supports fixed-length lookbehind.
//
// Anything else that has no exact equivalent throws, so a library upgrade that
// introduces new syntax fails loudly at generation time.

// Lookaround and backreferences need fancy-regex; everything else compiles with `regex`.
const FANCY = /\(\?(?:=|!|<=|<!)|\\[1-9]/;

function translate(regex) {
  const unsupportedFlags = regex.flags.replace(/[ig]/g, '');
  if (unsupportedFlags) {
    throw new Error(`unsupported flags ${regex.flags} in /${regex.source}/`);
  }

  const body = translateSource(regex.source);
  const pattern = regex.flags.includes('i') ? `(?i)${body}` : body;

  return { pattern, fancy: FANCY.test(pattern) };
}

function translateSource(source) {
  let out = '';
  let i = 0;
  let inClass = false;

  const fail = message => {
    throw new Error(`${message} at ${i} in /${source}/`);
  };

  while (i < source.length) {
    const c = source[i];

    if (c === '\\') {
      const d = source[i + 1];
      i += 2;

      switch (d) {
        case 'd':
          out += inClass ? '0-9' : '[0-9]';
          break;
        case 'w':
          out += inClass ? '0-9A-Za-z_' : '[0-9A-Za-z_]';
          break;
        case 'D':
          if (inClass) fail('\\D in class');
          out += '[^0-9]';
          break;
        case 'W':
          if (inClass) fail('\\W in class');
          out += '[^0-9A-Za-z_]';
          break;
        case 's':
        case 'S':
        case 'b':
        case 'B':
        case 'n':
        case 'r':
        case 't':
        case 'f':
        case 'v':
          out += `\\${d}`;
          break;
        case 'x':
          out += `\\x{${source.slice(i, i + 2)}}`;
          i += 2;
          break;
        case 'u':
          out += `\\x{${source.slice(i, i + 4)}}`;
          i += 4;
          break;
        case '/':
          out += '/';
          break;
        case '1':
        case '2':
        case '3':
        case '4':
        case '5':
        case '6':
        case '7':
        case '8':
        case '9':
          if (inClass) fail(`\\${d} in class`);
          out += `\\${d}`;
          break;
        default:
          if (d === undefined || /[0-9A-Za-z]/.test(d)) fail(`unsupported escape \\${d}`);
          out += `\\${d}`;
      }

      // `[\w-.]`: a hyphen right after a class escape is a literal in JS.
      if (inClass && 'dwsS'.includes(d) && source[i] === '-' && source[i + 1] !== ']') {
        out += '\\-';
        i += 1;
      }
      continue;
    }

    if (inClass) {
      if (c === ']') {
        inClass = false;
        out += ']';
      } else if (c === '[' || c === '&' || c === '~') {
        out += `\\${c}`;
      } else if (c === '-' && source[i + 1] === '-') {
        out += '\\-';
      } else {
        out += c;
      }
      i += 1;
      continue;
    }

    if (c === '[') {
      inClass = true;
      out += '[';
      i += 1;
      if (source[i] === '^') {
        out += '^';
        i += 1;
      }
      if (source[i] === ']') fail('empty class');
      continue;
    }

    if (c === '{') {
      const quantifier = /^\{\d+(?:,\d*)?\}/.exec(source.slice(i));
      if (quantifier) {
        out += quantifier[0];
        i += quantifier[0].length;
      } else {
        out += '\\{';
        i += 1;
      }
      continue;
    }

    if (c === '}') {
      out += '\\}';
      i += 1;
      continue;
    }

    if (source.startsWith('(?=', i) || source.startsWith('(?!', i)) {
      // JS allows quantified lookahead: `(?=x)?` / `(?=x)*` always succeed, so they
      // are dropped; `(?=x)+` is the bare assertion.
      const close = findClose(source, i);
      const quantifier = /^(?:[?*+]|\{\d+(?:,\d*)?\})\??/.exec(source.slice(close + 1));
      const group = `(?${source[i + 2]}${translateSource(source.slice(i + 3, close))})`;

      if (!quantifier) {
        out += group;
      } else if (!/^[?*]|^\{0/.test(quantifier[0])) {
        out += group;
      }
      i = close + 1 + (quantifier ? quantifier[0].length : 0);
      continue;
    }

    if (source.startsWith('(?<!', i) || source.startsWith('(?<=', i)) {
      const close = findClose(source, i);
      const branches = splitTopLevel(source.slice(i + 4, close));

      if (branches.length > 1 && source[i + 3] === '=') fail('positive lookbehind with alternation');

      out += branches.map(branch => `(?<${source[i + 3]}${translateSource(branch)})`).join('');
      i = close + 1;
      continue;
    }

    out += c;
    i += 1;
  }

  if (inClass) fail('unterminated class');

  return out;
}

// Index of the `)` closing the group that opens at `open`.
function findClose(source, open) {
  let depth = 0;
  let inClass = false;

  for (let i = open; i < source.length; i++) {
    const c = source[i];

    if (c === '\\') {
      i += 1;
    } else if (inClass) {
      if (c === ']') inClass = false;
    } else if (c === '[') {
      inClass = true;
    } else if (c === '(') {
      depth += 1;
    } else if (c === ')') {
      depth -= 1;
      if (depth === 0) return i;
    }
  }

  throw new Error(`unbalanced group in /${source}/`);
}

// Splits on `|` that is not nested in a group or class.
function splitTopLevel(source) {
  const parts = [];
  let depth = 0;
  let inClass = false;
  let start = 0;

  for (let i = 0; i < source.length; i++) {
    const c = source[i];

    if (c === '\\') {
      i += 1;
    } else if (inClass) {
      if (c === ']') inClass = false;
    } else if (c === '[') {
      inClass = true;
    } else if (c === '(') {
      depth += 1;
    } else if (c === ')') {
      depth -= 1;
    } else if (c === '|' && depth === 0) {
      parts.push(source.slice(start, i));
      start = i + 1;
    }
  }

  parts.push(source.slice(start));

  return parts;
}

module.exports = { translate };
