// Node fallback for tools/brace_check.py — same job: only braces in balance,
// per file. Used when the machine has no python.
const fs = require('fs');

const files = process.argv.slice(2);
let bad = 0;

for (const f of files) {
  const s = fs.readFileSync(f, 'utf8');
  let out = '';
  let i = 0;
  const n = s.length;
  while (i < n) {
    const c = s[i];
    if (c === '/' && s[i + 1] === '/') {
      while (i < n && s[i] !== '\n') i++;
      continue;
    }
    if (c === '/' && s[i + 1] === '*') {
      i += 2;
      while (i < n && !(s[i] === '*' && s[i + 1] === '/')) i++;
      i += 2;
      continue;
    }
    if (c === '"') {
      i++;
      while (i < n && s[i] !== '"') {
        if (s[i] === '\\') i++;
        i++;
      }
      i++;
      continue;
    }
    if (c === "'") {
      if (s[i + 1] === '\\') { i += 3; continue; }
      if (s[i + 2] === "'") { i += 3; continue; }
      i++;
      continue;
    }
    if (c === 'r' && s[i + 1] === '#' && s[i + 2] === '"') {
      // raw string r#"..."#
      const closer = '"' + s[i + 1];
      i += 3;
      while (i < n && !(s[i] === '"' && s[i + 1] === '#')) i++;
      i += 2;
      continue;
    }
    out += c;
    i++;
  }

  const pairs = { '{': '}', '(': ')', '[': ']' };
  const closers = new Set(Object.values(pairs));
  const stack = [];
  let ok = true;
  for (const ch of out) {
    if (pairs[ch]) stack.push(pairs[ch]);
    else if (closers.has(ch)) {
      if (stack.pop() !== ch) {
        console.log(f, 'MISMATCH on', ch);
        ok = false;
        bad++;
        break;
      }
    }
  }
  if (ok && stack.length) {
    console.log(f, 'UNCLOSED', stack.slice(-3));
    bad++;
  }
}

console.log(bad ? 'FAILED' : 'BRACES OK');
process.exit(bad ? 1 : 0);
