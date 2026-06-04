export function textEl(tag, text, className = "") {
  const node = document.createElement(tag);
  if (className) node.className = className;
  node.textContent = text || "";
  return node;
}

export function el(tag, className = "") {
  const node = document.createElement(tag);
  if (className) node.className = className;
  return node;
}

export function code(content) {
  const pre = el("pre", "studio-code");
  pre.textContent = content || "";
  return pre;
}
