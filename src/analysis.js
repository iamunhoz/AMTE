/** Pure document-wide analysis. It deliberately runs only in analysis-worker.js. */
export function numberHeadings(headings) {
  const counters = [0, 0, 0, 0, 0];
  return headings.map((heading) => {
    const index = heading.level - 1;
    counters[index] += 1;
    counters.fill(0, index + 1);
    return { ...heading, number: counters.slice(0, index + 1).filter(Boolean).join('.') };
  });
}

export function extractHeadings(markdown) {
  const headings = [];
  let fence = null;
  let offset = 0;
  for (const line of markdown.split('\n')) {
    const fenceMatch = line.match(/^\s*(`{3,}|~{3,})/);
    if (fenceMatch) {
      const marker = fenceMatch[1];
      if (!fence) fence = marker;
      else if (marker[0] === fence[0] && marker.length >= fence.length) fence = null;
    } else if (!fence) {
      const heading = line.match(/^(#{1,5})(?=\s|$)(?:\s+(.*))?$/);
      if (heading) headings.push({
        level: heading[1].length,
        text: (heading[2] || '').replace(/\s+#+\s*$/, '').trim(),
        position: offset,
      });
    }
    offset += line.length + 1;
  }
  return numberHeadings(headings);
}

export function wordCount(markdown) {
  const prose = markdown
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/\[([^\]]+)\]\([^)]*\)/g, '$1')
    .replace(/^\s{0,3}(?:[-*+]|\d+[.)])\s+/gm, '')
    .replace(/^\s{0,3}#{1,6}\s+/gm, '')
    .replace(/^\s{0,3}>\s?/gm, '')
    .replace(/`([^`]*)`/g, '$1')
    .replace(/[*_~]+/g, '')
    .replace(/<?https?:\/\/[^\s)>]+>?/g, '');
  return (prose.match(/[\p{L}\p{N}]+(?:['’\-][\p{L}\p{N}]+)*/gu) || []).length;
}

export function applyChanges(document, changes) {
  return [...changes]
    .sort((left, right) => right.from - left.from)
    .reduce((next, change) => next.slice(0, change.from) + change.inserted + next.slice(change.to), document);
}
