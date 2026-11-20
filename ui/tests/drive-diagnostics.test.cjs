const test = require('node:test');
const assert = require('node:assert/strict');
const { outputDirection, describeFault } = require('../drive-diagnostics.js');

test('direction comes from bit 1 of a confirmed word, including a valid zero', () => {
  assert.equal(outputDirection(null), null);
  assert.equal(outputDirection(undefined), null);
  assert.equal(outputDirection(0), 0);
  assert.equal(outputDirection(1), 0);
  assert.equal(outputDirection(0x1042), 1);
});

test('alarm state takes precedence over missing or not-yet-updated fault code', () => {
  for (const code of [0, null]) {
    assert.equal(describeFault(6, code, []).active, true);
    assert.match(describeFault(6, code, []).detail, /2100H = 6/);
  }
  assert.equal(describeFault(4, null, []).label, 'Нет данных');
  assert.equal(describeFault(null, null, []).label, 'Нет данных');
  assert.equal(describeFault(4, 0, []).label, '00 · Норма');
});

test('a reported fault remains identifiable when state or its catalog entry is missing', () => {
  const catalog = [{ code: 'Err14', description: 'Перегрев', action: 'Проверить охлаждение' }];
  const known = describeFault(null, 14, catalog);
  assert.equal(known.active, true);
  assert.equal(known.detail, catalog[0].description);
  assert.equal(known.action, catalog[0].action);
  assert.match(describeFault(4, 99, catalog).label, /Err99/);
});
