import { applyChanges, extractHeadings, wordCount } from './analysis.js';

export class AnalysisEngine {
  constructor(post, delay = 225) {
    this.post = post;
    this.delay = delay;
    this.document = '';
    this.version = 0;
    this.timer = null;
  }

  receive(message) {
    if (message.type === 'flush') {
      this.version = message.version;
      clearTimeout(this.timer);
      this.timer = null;
      this.analyze();
      return;
    }
    if (message.type === 'init') this.document = message.document;
    if (message.type === 'changes') this.document = applyChanges(this.document, message.changes);
    this.version = message.version;
    this.schedule();
  }

  schedule() {
    clearTimeout(this.timer);
    this.timer = setTimeout(() => this.analyze(), this.delay);
  }

  analyze() {
    this.timer = null;
    const documentVersion = this.version;
    const document = this.document;
    this.post({ type: 'analysis', version: documentVersion, total: wordCount(document), headings: extractHeadings(document) });
  }

  terminate() { clearTimeout(this.timer); }
}

if (typeof self !== 'undefined') {
  const engine = new AnalysisEngine((message) => self.postMessage(message));
  self.onmessage = ({ data }) => engine.receive(data);
}
