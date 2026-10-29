// The threaded web build's sound output (audio.rs `web_output::worklet`): on
// the browser's audio thread, plays what the mixer worker wrote to a ring in
// the module's shared memory, so mixing never waits for the page.
class HorizonMixer extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = options.processorOptions;
    // [frames read, frames written]: counters that wrap at 2^32.
    this.indices = new Int32Array(o.memory, o.indices, 2);
    // Interleaved stereo, `frames` (a power of two) long.
    this.samples = new Float32Array(o.memory, o.samples, o.frames * 2);
    this.mask = o.frames - 1;
    this.running = true;
    this.port.onmessage = () => {
      this.running = false;
    };
  }
  process(inputs, outputs) {
    const [left, right] = outputs[0];
    const read = Atomics.load(this.indices, 0);
    const queued = (Atomics.load(this.indices, 1) - read) >>> 0;
    const n = Math.min(left.length, queued);
    for (let i = 0; i < n; i++) {
      const at = ((read + i) & this.mask) * 2;
      left[i] = this.samples[at];
      if (right) right[i] = this.samples[at + 1];
    }
    // The mixer fell behind: the rest of the quantum stays silent.
    Atomics.store(this.indices, 0, (read + n) | 0);
    return this.running;
  }
}
registerProcessor("horizon-mixer", HorizonMixer);
