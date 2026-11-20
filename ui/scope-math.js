// Pure graph coordinate helpers shared by the Tauri WebView and Node regression tests.
(function (root) {
  'use strict';

  const scales = Object.freeze({ f: 1, i: 3, dc: 0.1, outputVolt: 0.1 });
  const channels = Object.entries(scales);

  function rangeForSamples(samples) {
    let maximum = 60;
    for (const sample of samples) {
      for (const [key, scale] of channels) {
        if (Number.isFinite(sample[key])) {
          maximum = Math.max(maximum, Math.abs(sample[key] * scale));
        }
      }
    }
    return Math.ceil(maximum / 10) * 10;
  }

  function yCoordinate(value, axisMaximum, height) {
    // +axisMaximum -> top; 0 -> middle; -axisMaximum -> bottom.
    if (!Number.isFinite(value) || !(axisMaximum > 0)) return NaN;
    return 16 + ((axisMaximum - value) / (2 * axisMaximum)) * (height - 32);
  }

  const api = Object.freeze({ scales, rangeForSamples, yCoordinate });
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.A650ScopeMath = api;
})(typeof window !== 'undefined' ? window : globalThis);
