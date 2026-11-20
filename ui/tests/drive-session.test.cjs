const test = require('node:test');
const assert = require('node:assert/strict');
const DriveSessionGate = require('../drive-session.js');

function deferred() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}

const status = (id, slave, simulated = false) => ({
  session_id: id, port_name: 'COM7', slave_id: slave, simulated, connected: !simulated, write_enabled: false
});

test('edits block every operation immediately, including a group read in flight', async () => {
  const reply = deferred();
  const calls = [];
  const gate = new DriveSessionGate(async (command, args) => {
    calls.push([command, args]);
    if (command === 'connect_serial') return status(args.slaveId, args.slaveId);
    if (command === 'read_registers_fc03') return reply.promise;
  });
  const old = await gate.transition('connect_serial', { slaveId: 1 });
  const read = gate.request(old, 'read_registers_fc03', { address: 0x3002 });
  const disconnect = gate.transition('disconnect_serial');
  for (const cmd of ['read_registers_fc03', 'write_register_fc06', 'poll_drive_telemetry', 'set_safety_allow_write']) {
    await assert.rejects(gate.request(old, cmd));
  }
  await disconnect;
  const active = await gate.transition('connect_serial', { slaveId: 2 });
  reply.resolve({ values: [380] });
  await assert.rejects(read);
  await assert.rejects(gate.request(old, 'get_connection_status'));
  await gate.request(active, 'get_connection_status');
  assert.deepEqual(calls.map(([command]) => command), [
    'connect_serial', 'read_registers_fc03', 'disconnect_serial', 'connect_serial', 'get_connection_status'
  ]);
  assert.equal(calls[1][1].sessionId, 1);
  assert.equal(calls.at(-1)[1].sessionId, 2);
});

test('a slow scan cannot restore the previous selection or disconnect the new session', async () => {
  const started = deferred(), found = deferred(), calls = [];
  const gate = new DriveSessionGate(async command => {
    calls.push(command);
    if (command === 'auto_scan_drive') { started.resolve(); return found.promise; }
    if (command === 'connect_serial') return status(19, 1, true);
  });
  const scan = gate.transition('auto_scan_drive');
  await started.promise;
  const changed = gate.transition('disconnect_serial');
  const next = gate.transition('connect_serial');
  assert.equal(gate.active, null);
  found.resolve(status(18, 1));
  assert.equal(await scan, null);
  await changed;
  assert.equal((await next).session_id, 19);
  assert.deepEqual(calls, ['auto_scan_drive', 'connect_serial']);
});

test('failed or unconfirmed connections stay locked and can be retried', async () => {
  const gate = new DriveSessionGate(async (_cmd, { result }) => {
    if (result instanceof Error) throw result;
    return result;
  });
  for (const result of [new Error('timeout'), { ...status(2, 1), connected: false }, { simulated: true }]) {
    await assert.rejects(gate.transition('connect_serial', { result }));
    await assert.rejects(gate.request(gate.active, 'set_safety_allow_write'));
  }
  assert.equal((await gate.transition('connect_serial', { result: status(3, 1) })).session_id, 3);
});
