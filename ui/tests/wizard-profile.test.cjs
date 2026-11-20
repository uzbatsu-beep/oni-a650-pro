const test = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const path = require('node:path');
const { pumpSettings, pidSettings, makePlan, applyPlan, profileRows } = require('../wizard-profile.js');

const html = readFileSync(path.join(__dirname, '../index.html'), 'utf8');
const catalog = JSON.parse(html.match(/const PARAM_CATALOG = (\[.*\]);/)[1]);
const byId = new Map(catalog.map(p => [p.id, p]));
const defaults = Object.fromEntries(catalog.map(p => [p.id, p.def]));
const pump = { enabled: true, mode: '1', rotation: '2', minutes: 240, clean: false,
  fill: true, fillRate: 1.2, fillTarget: 25, dryRun: true, fire: '' };

test('all four pump modes and optional settings export exactly the preview on a clean profile', () => {
  for (const mode of ['0','1','2','3']) for (const rotation of ['0','1','2']) {
    for (const fill of [false,true]) for (const dryRun of [false,true]) {
      for (const fire of ['', '0', '1', '2', '3', '4']) {
        const settings = pumpSettings({ ...pump, mode, rotation, fill, dryRun, fire });
        const plan = makePlan(settings, byId);
        const values = { ...defaults }, explicit = new Set();
        applyPlan(plan, values, explicit);
        assert.deepEqual(profileRows(catalog, values, explicit).sort(), plan.map(r => r.id + ' = ' + r.raw).sort());
        assert.equal(values['H00.00'], '1');
        assert.equal(values['H00.01'], mode);
        assert.equal(explicit.has('H00.13'), rotation === '2');
        assert.equal(explicit.has('H00.24'), fill);
        assert.equal(explicit.has('H00.28'), dryRun);
        assert.equal(explicit.has('H00.42'), fire !== '');
      }
    }
  }
});

test('turning off the cascade changes only H00.00 and exports its explicit zero', () => {
  const values = { ...defaults, 'H00.42': '3' }, explicit = new Set();
  const plan = makePlan(pumpSettings({ ...pump, enabled: false }), byId);
  assert.deepEqual(plan.map(p => p.id), ['H00.00']);
  applyPlan(plan, values, explicit);
  assert.deepEqual(profileRows(catalog, values, explicit), ['H00.00 = 0', 'H00.42 = 3']);
});

test('PID tuning and zero loss threshold survive conversion to ODS', () => {
  const values = { ...defaults }, explicit = new Set();
  const plan = makePlan(pidSettings({ range: 10, target: 4, kp: 1.8, ti: 1.5, loss: 0 }), byId);
  applyPlan(plan, values, explicit);
  const rows = profileRows(catalog, values, explicit);
  for (const match of ['F13.01 = 400','F13.08 = 18','F13.09 = 150','F13.24 = 0']) assert.ok(rows.includes(match));
  assert.ok(!rows.some(r => r.startsWith('F13.07') || r.startsWith('F13.21')));
});

test('invalid codes, increments, blank input and invalid pressure cannot yield a plan', () => {
  for (const change of [{mode:'4'},{mode:''},{minutes:-1},{minutes:1.5},{fillRate:NaN}]) {
    assert.throws(() => makePlan(pumpSettings({ ...pump, ...change }), byId));
  }
  assert.throws(() => pidSettings({ range: 6, target: 7 }));
});
