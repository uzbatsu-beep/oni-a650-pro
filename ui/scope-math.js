// Pure graph coordinate helpers shared by the Tauri WebView and Node regression tests.
(function (root) {
  'use strict';

  // F00 U/I/DC overlay values are scaled in the UI (current ×3, DC bus ÷10).
  // The signed frequency is intentionally NOT clamped to zero.
  function rangeForSamples(samples) {
    let maximum = 60;
    for (const sample of samples) {
      for (const [key, scale] of [['f', 1], ['i', 3], ['dc', 0.1]]) {
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

  const api = Object.freeze({ rangeForSamples, yCoordinate });
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.A650ScopeMath = api;
})(typeof window !== 'undefined' ? window : globalThis);
