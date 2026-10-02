class PCM16Processor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.step = sampleRate / 16000;
    this.nextSample = 0;
    this.inputIndex = 0;
    this.previous = 0;
    this.pending = new Int16Array(1600);
    this.count = 0;
    this.running = true;
    // Level reporting: at most one report per ~33 ms of incoming audio, measured
    // from the same PCM stream the recogniser hears.
    this.peak = 0;
    this.framesSinceLevel = 0;
    this.levelInterval = Math.round(sampleRate * 0.033);
    this.port.onmessage = (event) => {
      if (event.data === 'stop') {
        this.flush();
        this.running = false;
        this.port.postMessage({ type: 'ended' });
      }
    };
  }

  flush() {
    if (!this.count) return;
    const chunk = this.pending.slice(0, this.count);
    this.port.postMessage({ type: 'audio', buffer: chunk.buffer }, [chunk.buffer]);
    this.count = 0;
  }

  reportLevel() {
    this.framesSinceLevel = 0;
    const level = Math.min(1, this.peak / 32768);
    this.peak = 0;
    this.port.postMessage({ type: 'level', level });
  }

  process(inputs) {
    if (!this.running) return false;
    const channels = inputs[0];
    if (!channels?.length) return true;
    const length = channels[0].length;
    for (let i = 0; i < length; i++) {
      let current = 0;
      for (const channel of channels) current += channel[i] / channels.length;
      const index = this.inputIndex++;
      while (this.nextSample <= index) {
        const fraction = index ? this.nextSample - (index - 1) : 1;
        const value = Math.max(-1, Math.min(1, this.previous + (current - this.previous) * fraction));
        const sample = Math.round(value < 0 ? value * 32768 : value * 32767);
        const magnitude = Math.abs(sample);
        if (magnitude > this.peak) this.peak = magnitude;
        this.pending[this.count++] = sample;
        if (this.count === this.pending.length) this.flush();
        this.nextSample += this.step;
      }
      this.previous = current;
    }
    this.framesSinceLevel += length;
    if (this.framesSinceLevel >= this.levelInterval) this.reportLevel();
    return true;
  }
}
registerProcessor('pcm-16k', PCM16Processor);
