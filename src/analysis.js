/** Pure document-wide analysis. It deliberately runs only in analysis-worker.js. */
export const WORD_TOKEN_PATTERN = /[\p{L}\p{N}]+(?:['’\-][\p{L}\p{N}]+)*/gu;
const WORD_CONTEXT_PATTERN = /^[\p{L}\p{N}'’\-]$/u;
export function numberHeadings(headings) {
  const counters = [0, 0, 0, 0, 0, 0];
  return headings.map((heading) => {
    const index = heading.level - 1;
    counters[index] += 1;
    counters.fill(0, index + 1);
    return { ...heading, number: counters.slice(0, index + 1).filter(Boolean).join('.') };
  });
}

function headingFingerprint(text) {
  // Keep the persisted identity small and independent of a source offset. The
  // full normalized title participates in the hash, while occurrence makes
  // otherwise identical headings distinct within a document.
  const normalized = text.normalize('NFC').trim().replace(/\s+/g, ' ').toLowerCase();
  let hash = 0xcbf29ce484222325n;
  for (const character of normalized) {
    hash ^= BigInt(character.codePointAt(0));
    hash = BigInt.asUintN(64, hash * 0x100000001b3n);
  }
  return hash.toString(16).padStart(16, '0');
}

function headingBase(heading) {
  return `h${heading.level}:${heading.titleFingerprint}`;
}

/**
 * Adds the document-order hierarchy without inventing skipped levels.  The
 * stack contains only real headings, so its top after higher/equal headings
 * have been removed is exactly the closest preceding lower-level parent.
 */
export function buildHeadingTree(headings) {
  const stack = [];
  const occurrences = new Map();
  const tree = headings.map((heading, index) => {
    const titleFingerprint = headingFingerprint(heading.text);
    const base = `h${heading.level}:${titleFingerprint}`;
    const occurrence = (occurrences.get(base) || 0) + 1;
    occurrences.set(base, occurrence);
    while (stack.length && stack.at(-1).level >= heading.level) stack.pop();
    const parentIndex = stack.at(-1)?.index ?? null;
    const node = {
      ...heading,
      index,
      titleFingerprint,
      occurrence,
      identity: `${base}:${occurrence}`,
      parentIndex,
      parentIdentity: parentIndex === null ? null : headings[parentIndex]?.identity || null,
      childCount: 0,
      context: '',
    };
    // `headings` is the same array passed by callers, but it has not yet been
    // decorated. Retain parent identity locally and fill it after every node
    // has been constructed below.
    stack.push({ index, level: heading.level });
    return node;
  });
  for (const node of tree) {
    if (node.parentIndex !== null) {
      node.parentIdentity = tree[node.parentIndex].identity;
      tree[node.parentIndex].childCount += 1;
    }
  }
  for (let index = 0; index < tree.length; index += 1) {
    const before = tree[index - 1];
    const after = tree[index + 1];
    const parent = nodeParent(tree, tree[index]);
    // Context is not persisted: it lets runtime reconciliation distinguish a
    // duplicate whose ordinal changed from an unrelated duplicate.
    tree[index].context = [
      parent ? headingBase(parent) : 'root',
      before ? headingBase(before) : '^',
      after ? headingBase(after) : '$',
    ].join('|');
  }
  return tree;
}

function nodeParent(tree, node) {
  return node.parentIndex === null ? null : tree[node.parentIndex] || null;
}

/**
 * Carries collapsed state only when a surviving heading can be matched
 * unambiguously. Unique title/level nodes survive ordinal changes; duplicate
 * nodes additionally require the same immediate structural context. This
 * avoids restoring a fold onto a different duplicate after insert/reorder.
 */
export function reconcileCollapsedChapterIdentities(previous, next, collapsed) {
  const wanted = new Set(Array.isArray(collapsed) ? collapsed : []);
  const oldByBase = new Map();
  const nextByBase = new Map();
  for (const heading of previous || []) {
    const base = headingBase(heading);
    oldByBase.set(base, [...(oldByBase.get(base) || []), heading]);
  }
  for (const heading of next || []) {
    const base = headingBase(heading);
    nextByBase.set(base, [...(nextByBase.get(base) || []), heading]);
  }
  const result = new Set();
  for (const [base, oldNodes] of oldByBase) {
    const folded = oldNodes.filter((node) => wanted.has(node.identity));
    if (!folded.length || !nextByBase.has(base)) continue;
    const newNodes = nextByBase.get(base);
    if (oldNodes.length === 1 && newNodes.length === 1) {
      if (newNodes[0].childCount) result.add(newNodes[0].identity);
      continue;
    }
    for (const oldNode of folded) {
      const candidates = newNodes.filter((node) => node.context === oldNode.context);
      if (candidates.length === 1 && candidates[0].childCount) result.add(candidates[0].identity);
    }
  }
  // A recovered state has no prior model. Its canonical identities are still
  // safe to accept when they name a current parent; stale leaves are pruned.
  if (!previous?.length) {
    for (const node of next || []) if (wanted.has(node.identity) && node.childCount) result.add(node.identity);
  }
  return [...result].sort();
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
      const heading = line.match(/^(#{1,6})(?=\s|$)(?:\s+(.*))?$/);
      if (heading) headings.push({
        level: heading[1].length,
        text: (heading[2] || '').replace(/\s+#+\s*$/, '').trim(),
        position: offset,
      });
    }
    offset += line.length + 1;
  }
  return buildHeadingTree(numberHeadings(headings));
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
  return (prose.match(WORD_TOKEN_PATTERN) || []).length;
}

export function wordTokenRanges(text, offset = 0) {
  const ranges = [];
  WORD_TOKEN_PATTERN.lastIndex = 0;
  for (let match = WORD_TOKEN_PATTERN.exec(text); match; match = WORD_TOKEN_PATTERN.exec(text)) {
    ranges.push({ from: offset + match.index, to: offset + match.index + match[0].length });
  }
  return ranges;
}

function codePointBefore(read, position) {
  return [...read(Math.max(0, position - 2), position)].at(-1) || '';
}

function codePointAfter(read, position) {
  return [...read(position, position + 2)][0] || '';
}

function wordContextBefore(read, position) {
  return WORD_CONTEXT_PATTERN.test(codePointBefore(read, position));
}

function wordContextAfter(read, position) {
  return WORD_CONTEXT_PATTERN.test(codePointAfter(read, position));
}

function expandWordContext(read, length, from, to) {
  let start = from;
  let end = to;
  while (start > 0 && wordContextBefore(read, start)) start -= codePointBefore(read, start).length;
  while (end < length && wordContextAfter(read, end)) end += codePointAfter(read, end).length;
  return { from: start, to: end };
}

function tokensInRanges(read, ranges) {
  const tokens = new Map();
  for (const range of ranges) {
    for (const token of wordTokenRanges(read(range.from, range.to), range.from)) {
      if (token.to > range.from && token.from < range.to) tokens.set(`${token.from}:${token.to}`, token);
    }
  }
  return [...tokens.values()].sort((left, right) => left.from - right.from || left.to - right.to);
}

function mapBeforePosition(changes, position, association) {
  let delta = 0;
  for (const change of changes) {
    if (position < change.from || (position === change.from && association < 0)) break;
    if (position > change.to || (position === change.to && association > 0)) {
      delta += change.inserted.length - (change.to - change.from);
      continue;
    }
    return null;
  }
  return position + delta;
}

function survivingSegments(token, changes) {
  const segments = [];
  let cursor = token.from;
  for (const change of changes) {
    if (change.from > token.to) break;
    if (change.from > cursor) segments.push({ from: cursor, to: Math.min(change.from, token.to) });
    if (change.to > cursor) cursor = Math.min(change.to, token.to);
  }
  if (cursor < token.to) segments.push({ from: cursor, to: token.to });
  return segments;
}

/**
 * Classifies only the word tokens that touch CodeMirror change ranges. A token
 * with any unchanged character is retained (one-to-one, leftmost tie break);
 * unmatched old/new tokens are deleted/added. This keeps partial in-token
 * edits neutral while treating a full replacement as one deletion plus one
 * addition, without scanning the rest of the document.
 */
export function classifyWordActivity({ beforeLength, afterLength, readBefore, readAfter, changes }) {
  const normalized = [...changes]
    .map((change) => ({ ...change, inserted: String(change.inserted || '') }))
    .sort((left, right) => left.from - right.from || left.to - right.to);
  if (!normalized.length) return { added: 0, deleted: 0 };

  const beforeRanges = normalized.map((change) => expandWordContext(readBefore, beforeLength, change.from, change.to));
  const afterRanges = normalized.map((change) => expandWordContext(readAfter, afterLength, change.fromB, change.toB));
  const beforeTokens = tokensInRanges(readBefore, beforeRanges);
  const afterTokens = tokensInRanges(readAfter, afterRanges);
  const relationships = [];

  beforeTokens.forEach((beforeToken, beforeIndex) => {
    for (const segment of survivingSegments(beforeToken, normalized)) {
      const from = mapBeforePosition(normalized, segment.from, 1);
      const to = mapBeforePosition(normalized, segment.to, -1);
      if (from === null || to === null || to <= from) continue;
      afterTokens.forEach((afterToken, afterIndex) => {
        const overlap = Math.max(0, Math.min(to, afterToken.to) - Math.max(from, afterToken.from));
        if (overlap) relationships.push({ beforeIndex, afterIndex, overlap, from: afterToken.from });
      });
    }
  });

  // Several retained pieces can describe the same pair; retain the strongest
  // correspondence before choosing one survivor for split/join edits.
  const scores = new Map();
  for (const relation of relationships) {
    const key = `${relation.beforeIndex}:${relation.afterIndex}`;
    const previous = scores.get(key);
    scores.set(key, previous ? { ...previous, overlap: previous.overlap + relation.overlap } : relation);
  }
  const matchedBefore = new Set();
  const matchedAfter = new Set();
  for (const relation of [...scores.values()].sort((left, right) => right.overlap - left.overlap
    || left.from - right.from || left.beforeIndex - right.beforeIndex || left.afterIndex - right.afterIndex)) {
    if (matchedBefore.has(relation.beforeIndex) || matchedAfter.has(relation.afterIndex)) continue;
    matchedBefore.add(relation.beforeIndex);
    matchedAfter.add(relation.afterIndex);
  }
  return { added: afterTokens.length - matchedAfter.size, deleted: beforeTokens.length - matchedBefore.size };
}

export function applyChanges(document, changes) {
  return [...changes]
    .sort((left, right) => right.from - left.from)
    .reduce((next, change) => next.slice(0, change.from) + change.inserted + next.slice(change.to), document);
}
