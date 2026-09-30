// Node port of tools/brace_check.py for machines without python.
// Same two checks: unclosed delimiters, and a string literal that never closes.
import fs from "node:fs";
import path from "node:path";

const root = path.resolve(import.meta.dirname, "..", "rust", "src");
const CHAR_LITERAL = /'(?:\\.|[^\\'])'/y;
const OPEN = new Set(["{", "(", "["]);
const MATCH = { "}": "{", ")": "(", "]": "[" };

function* tokens(text) {
  let index = 0;
  let line = 1;
  const length = text.length;
  while (index < length) {
    const char = text[index];
    if (char === "\n") {
      yield [line, "\n"];
      line++;
      index++;
      continue;
    }
    if (char === "/" && text[index + 1] === "/") {
      const nl = text.indexOf("\n", index);
      if (nl < 0) return;
      index = nl;
      continue;
    }
    if (char === '"') {
      yield [line, '"'];
      index++;
      while (index < length) {
        if (text[index] === "\\") {
          if (text[index + 1] === "\n") line++;
          index += 2;
          continue;
        }
        if (text[index] === "\n") {
          yield [line, "\n"];
          line++;
          index++;
          continue;
        }
        if (text[index] === '"') {
          yield [line, '"'];
          index++;
          break;
        }
        index++;
      }
      continue;
    }
    if (char === "'") {
      CHAR_LITERAL.lastIndex = index;
      const match = CHAR_LITERAL.exec(text);
      if (match) {
        index = match.index + match[0].length;
        continue;
      }
    }
    if (OPEN.has(char) || MATCH[char]) yield [line, char];
    index++;
  }
}

let broken = 0;
for (const name of fs.readdirSync(root).filter((f) => f.endsWith(".rs")).sort()) {
  const full = path.join(root, name);
  const text = fs.readFileSync(full, "utf8");
  const lines = text.split("\n");
  const found = [...tokens(text)];

  const stack = [];
  for (const [line, char] of found) {
    if (OPEN.has(char)) stack.push([line, char]);
    else if (char === '"' || char === "\n") continue;
    else if (stack.length && stack[stack.length - 1][1] === MATCH[char]) stack.pop();
    else if (MATCH[char]) stack.push([line, char]);
  }
  const problems = stack.map(
    ([line, char]) => `${name}:${line} unclosed ${JSON.stringify(char)}  ${lines[line - 1].trim().slice(0, 70)}`,
  );

  let start = null;
  for (const [line, char] of found) if (char === '"') start = start === null ? line : null;
  if (start !== null) problems.push(`${name}:${start} string literal never closes`);

  if (problems.length) {
    broken++;
    console.log(problems.join("\n"));
  }
}
if (!broken) console.log("clean");
