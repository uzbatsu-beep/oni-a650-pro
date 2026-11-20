const test = require('node:test');
const assert = require('node:assert/strict');
const { scales, rangeForSamples, yCoordinate } = require('../scope-math.js');

test('Scope: reverse frequency is below zero, forward is above zero', () => {
  const h = 256;
  const max = rangeForSamples([{ f: -50, i: 12.5, dc: 538 }, { f: 50, i: 10, dc: 540 }]);
  assert.equal(max, 60);
  const zeroY = yCoordinate(0, max, h);
  assert.equal(zeroY, h / 2);
  assert.ok(yCoordinate(-50, max, h) > zeroY);
  assert.ok(yCoordinate(50, max, h) < zeroY);
  assert.ok(yCoordinate(53.8, max, h) < zeroY); // DC is still above zero
  assert.ok(yCoordinate(37.5, max, h) < zeroY); // current is still above zero
});

test('Scope: out-of-range frequency automatically expands symmetrical axis', () => {
  const max = rangeForSamples([{ f: -150, i: 1, dc: 500 }, { f: 175 }]);
  assert.equal(max, 180);
  assert.equal(yCoordinate(-180, max, 256), 240);
  assert.equal(yCoordinate(180, max, 256), 16);
});

test('Scope: missing telemetry is ignored when sizing axes', () => {
  assert.equal(rangeForSamples([{ f: null, i: NaN, dc: undefined }]), 60);
  assert.ok(Number.isNaN(yCoordinate(NaN, 60, 256)));
});

test('Scope: output voltage uses V/10 independently of the DC bus', () => {
  assert.equal(380 * scales.outputVolt, 38);
  assert.equal(rangeForSamples([{ f: -50, outputVolt: 380, dc: 538 }]), 60);
  assert.equal(rangeForSamples([{ outputVolt: null }, { outputVolt: 790, dc: 538 }]), 80);
  assert.equal(rangeForSamples([{ outputVolt: 0 }, { outputVolt: NaN }]), 60);
});
