// All device addresses and writable candidates are supplied by the Rust session.
function setupEepromDialog({ active, native, canWrite, catalog, request, updateStatus }) {
  const el = name => document.getElementById('eeprom' + name);
  const dialog = el('Dialog');
  const rows = el('Rows');
  const status = el('Status');
  const hex = word => '0x' + word.toString(16).toUpperCase().padStart(4, '0');
  let session = null;
  let busy = false;
  let items = new Map();

  function sync() {
    if (!dialog.open) return;
    const connected = Boolean(native && session && active() === session);
    const count = rows.querySelectorAll('input:checked').length;
    el('Code').disabled = el('Add').disabled = busy || !connected;
    el('Close').disabled = busy;
    el('Save').disabled = busy || !connected || !canWrite() || count === 0;
    el('Save').textContent = 'Записать выбранные (' + count + ')';
    el('Lock').textContent = !native ? 'Запись доступна в настольном конфигураторе.'
      : !connected ? 'Для чтения и сохранения подключите ПЧ или симулятор.'
      : !canWrite() ? 'Для записи закройте окно и разрешите запись в SafetyGuard.' : '';
  }

  function render(selected) {
    rows.replaceChildren();
    if (!items.size) rows.textContent = 'Список для сохранения пуст.';
    for (const item of items.values()) {
      const p = catalog.get(item.code);
      const label = document.createElement('label');
      const check = document.createElement('input');
      check.type = 'checkbox';
      check.value = item.code;
      check.checked = selected.has(item.code);
      check.disabled = busy;
      const text = document.createElement('span');
      text.textContent = item.code + (p ? ' · ' + p.n : '');
      const detail = document.createElement('small');
      const value = p ? Number((WizardProfile.readRaw(p, item.raw) * p.sc).toFixed(3)) + ' ' + p.u : item.raw;
      detail.textContent = value + ' · RAM ' + hex(item.ram) + ' → EEPROM ' + hex(item.eeprom) + ' · raw ' + hex(item.raw);
      text.append(detail);
      label.append(check, text);
      rows.append(label);
    }
    sync();
  }

  const selectedCodes = () => new Set(Array.from(rows.querySelectorAll('input:checked'), e => e.value));
  const current = token => dialog.open && token === session && token === active();
  rows.addEventListener('change', sync);
  el('Open').addEventListener('click', async () => {
    const token = session = active();
    items = new Map();
    el('Codes').replaceChildren();
    el('Code').value = status.textContent = '';
    el('Device').textContent = token ? (token.simulated ? 'Симулятор · запись в память симулятора'
      : token.port_name + ' · ID ' + token.slave_id) : '';
    dialog.showModal();
    busy = Boolean(token && native);
    render(new Set());
    if (!busy) return;
    try {
      const preview = await request(token, 'eeprom_preview');
      if (!current(token)) return;
      items = new Map(preview.pending.map(item => [item.code, item]));
      for (const code of preview.available) {
        const option = document.createElement('option');
        option.value = code;
        option.label = catalog.get(code)?.n || code;
        el('Codes').append(option);
      }
    } catch (err) { if (current(token)) status.textContent = String(err); }
    finally { if (current(token)) { busy = false; render(new Set(items.keys())); } }
  });

  el('Stage').addEventListener('submit', async event => {
    event.preventDefault();
    const code = el('Code').value.trim().toUpperCase();
    const token = session;
    if (!code || busy || !token || !current(token)) return;
    const selected = selectedCodes();
    busy = true;
    render(selected);
    try {
      const item = await request(token, 'stage_eeprom_parameter', { code });
      if (!current(token)) return;
      items.set(item.code, item);
      selected.add(item.code);
      el('Code').value = '';
      status.textContent = 'Прочитано из RAM · ' + code;
    } catch (err) { if (current(token)) status.textContent = String(err); }
    finally { if (current(token)) { busy = false; render(selected); } }
  });

  el('Save').addEventListener('click', async () => {
    const token = session;
    const selected = selectedCodes();
    if (busy || !token || !current(token) || !canWrite() || !selected.size) return;
    busy = true;
    render(selected);
    const total = selected.size;
    let done = 0;
    let code;
    try {
      for (code of [...selected]) {
        status.textContent = 'Запись ' + code + ' (' + (done + 1) + ' из ' + total + ')';
        await request(token, 'persist_eeprom_parameter', { code, expectedRaw: items.get(code).raw });
        if (!current(token)) return;
        items.delete(code);
        selected.delete(code);
        done++;
      }
      status.textContent = (token.simulated ? 'Симулятор' : 'ПЧ') + ' подтвердил FC06 · ' + done + ' из ' + total;
    } catch (err) {
      if (!current(token)) return;
      status.textContent = 'Запись остановлена на ' + code + '. Подтверждено ' + done + ' из ' + total + '. ' + String(err);
      try {
        const connection = await request(token, 'get_connection_status');
        if (current(token)) updateStatus(connection);
        const preview = await request(token, 'eeprom_preview');
        if (current(token)) items = new Map(preview.pending.map(item => [item.code, item]));
      } catch (_) { /* Keep the outcome of the original FC06 visible. */ }
    } finally { if (current(token)) { busy = false; render(selected); } }
  });

  function dismiss() {
    if (dialog.open) dialog.close();
    session = null;
    busy = false;
  }
  el('Close').addEventListener('click', dismiss);
  dialog.addEventListener('cancel', event => { if (busy) event.preventDefault(); });
  return { dismiss, sync };
}
