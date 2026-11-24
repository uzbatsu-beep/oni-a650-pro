const test = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const path = require('node:path');
const { pumpSettings, pidSettings, makePlan, applyPlan, profileRows, profileText, readRaw } = require('../wizard-profile.js');

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
  const values = { ...defaults, 'H00.42': '3' }, explicit = new Set(['H00.42']);
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

test('ODS keeps selected setpoints separate from signed and unsigned FC03 values', () => {
  const values = { ...defaults, 'F03.01': '15.0', 'F06.01': '-1.2', 'U00.02': '380', 'F03.00': '12.0' };
  const explicit = new Set(['F03.01', 'F06.01']);
  const readings = new Map([['U00.02', readRaw(byId.get('U00.02'), 380)],
    ['U00.00', readRaw(byId.get('U00.00'), 60536)], ['F03.01', 17], ['F04.00', 0]]);
  const connection = { portName: 'COM7', slaveId: 23, baudRate: 19200, frameFormat: '1-8-E-1', simulated: false };
  const text = profileText('A650M', connection, true, catalog, values, explicit, readings);
  assert.equal(text.charCodeAt(0), 0xfeff);
  const lines = text.split('\r\n');
  assert.deepEqual(lines.slice(lines.indexOf('[Parameter Set]') + 1, lines.indexOf('[Parameter Read]') - 1),
    ['F03.01 = 150', 'F06.01 = -12']);
  assert.deepEqual(lines.slice(lines.indexOf('[Parameter Read]') + 1, lines.indexOf('[Check Parameter]') - 1),
    ['F03.01 = 17', 'F04.00 = 0', 'U00.00 = -5000', 'U00.02 = 380']);
  for (const field of ['Port = COM7', 'Address = 23', 'BaudRate = 19200', 'FrameFormat = 8E1',
    'Transport = RS-485', 'ConnectionSource = Confirmed']) assert.ok(lines.includes(field));
  for (const invalid of [-1, 65536, 0.5, null]) assert.throws(() => readRaw(byId.get('F03.01'), invalid));
});

test('manual profile validation rejects read-only, unknown, reserved, off-step and out-of-range values', () => {
  for (const [id, input] of [['U00.02', '380'], ['F99.99','1'], ['F05.02','24'], ['F03.01','-1'],
    ['F03.01','0.01'], ['F03.01',''], ['F08.01','----'], ['F03.01','Infinity']]) {
    assert.throws(() => profileRows(catalog, { ...defaults, [id]: input }, new Set([id])), { message: new RegExp(id) });
  }
  assert.deepEqual(profileRows(catalog, { ...defaults, 'U00.02': '380', 'F03.01': '3' }, new Set()), []);
});

test('local reference bounds use the current engineering values and require a defined limit', () => {
  const values = { ...defaults, 'F01.08': '60', 'F01.09': '55' };
  assert.deepEqual(profileRows(catalog, values, new Set(['F01.09'])), ['F01.09 = 5500']);
  for (const max of ['54.99', '----', '']) {
    assert.throws(() => profileRows(catalog, { ...values, 'F01.08': max }, new Set(['F01.09'])));
  }
});

test('offline headers describe selected settings and distinguish simulation', () => {
  const cfg = { portName: '', baudRate: 38400, frameFormat: '1-8-O-1', simulated: true, slaveId: 4 };
  const text = profileText('A650M', cfg, false, catalog, defaults, new Set(), new Map());
  assert.match(text, /Transport = Simulator\r\nConnectionSource = Selected/);
  assert.match(text, /FrameFormat = 8O1/);
  assert.doesNotMatch(text, /Port =|COM3|\r\nBaud =/);
  for (const change of [{ slaveId: 0 }, { baudRate: 0 }, { frameFormat: '' }, { portName: 'COM1\r\n' }]) {
    assert.throws(() => profileText('A650M', { ...cfg, ...change }, false, catalog, defaults, new Set(), new Map()));
  }
});

test('Ethernet profile records gateway, TCP port and station without COM settings', () => {
  const cfg = { mode: 'ethernet', ip: '192.168.1.10', port: 4001, slaveId: 17 };
  const text = profileText('A650M', cfg, true, catalog, defaults, new Set(), new Map());
  for (const field of ['Transport = RTU over Ethernet','IP = 192.168.1.10','TcpPort = 4001','Address = 17']) assert.ok(text.includes(field));
  assert.doesNotMatch(text, /BaudRate|FrameFormat|\r\nPort =/);
  assert.throws(() => profileText('A650M', {...cfg, port: 0}, true, catalog, defaults, new Set(), new Map()));
});

test('opening an ODS converts only intended setpoints and rejects invalid values atomically', () => {
  const { importPlan, browserProfileSet } = require('../wizard-profile.js');
  const text = '\uFEFF[General]\r\nModel = ПЧ ONI A650M\r\n[Parameter Set]\r\nF03.00 = 1\r\nF06.01 = -12\r\n[Parameter Read]\r\nU00.02 = 380\r\n';
  const plan = importPlan(browserProfileSet(text), byId, defaults);
  assert.deepEqual(plan.map(p=>[p.id,p.value]), [['F03.00','0.1'],['F06.01','-1.2']]);
  assert.throws(()=>importPlan({ 'F03.00': 1, 'U00.02': 380 },byId,defaults));
  assert.throws(()=>browserProfileSet(text.replace('A650M','A650')));
  assert.throws(()=>browserProfileSet(text+'[Parameter Set]\nF03.00 = 2'));
});

test('drive nameplate units agree in the UI and native parameter catalog', () => {
  const csv = readFileSync(path.join(__dirname, '../../src-tauri/data/parameters.csv'), 'utf8');
  for (const [code, unit] of [['F00.20','kW'],['F00.21','V'],['F00.22','A']]) {
    assert.equal(byId.get(code).u, unit);
    assert.ok(csv.split('\n').find(row=>row.startsWith(code+',')).includes(','+unit+','));
  }
});
