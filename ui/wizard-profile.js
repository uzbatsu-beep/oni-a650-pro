// Values in the editor are engineering units. Only the ODS boundary uses raw words.
(function (root) {
  'use strict';

  function pumpSettings(options) {
    const values = { 'H00.00': options.enabled ? 1 : 0 };
    if (!options.enabled) return values;
    values['H00.01'] = options.mode;
    values['H00.12'] = options.rotation;
    if (options.rotation === '2') values['H00.13'] = options.minutes;
    values['H00.15'] = options.clean ? 1 : 0;
    values['H00.23'] = options.fill ? 1 : 0;
    if (options.fill) {
      values['H00.24'] = options.fillRate;
      values['H00.25'] = options.fillTarget;
    }
    values['H00.26'] = options.dryRun ? 1 : 0;
    if (options.dryRun) {
      values['H00.27'] = 40;
      values['H00.28'] = 10;
    }
    if (options.fire !== '') values['H00.42'] = options.fire;
    return values;
  }

  function pidSettings({ range, target, kp, ti, loss }) {
    if (!Number.isFinite(target) || !Number.isFinite(range) || range <= 0 || target < 0 || target > range) {
      throw new Error('Уставка давления должна попадать в выбранную шкалу датчика.');
    }
    return {
      'F01.01': 6, 'F13.00': 0, 'F13.01': (100 * target / range).toFixed(1),
      'F13.02': 0, 'F13.03': range * 10, 'F13.08': kp, 'F13.09': ti, 'F13.24': loss
    };
  }

  function numeric(input) {
    if (typeof input === 'number') return input;
    if (typeof input !== 'string' || !/^[-+]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[-+]?\d+)?$/i.test(input.trim())) return NaN;
    return Number(input);
  }

  function resolveLimit(limit, values, catalog) {
    const literal = numeric(limit);
    if (Number.isFinite(literal)) return literal;
    const match = /^(-?)([A-Z]\d{2}\.\d{2})$/.exec(limit);
    if (match) {
      const id = match[2];
      const value = numeric(values[id] ?? catalog.get(id)?.def);
      if (Number.isFinite(value)) return match[1] ? -value : value;
    }
    throw new Error('Не определена граница ' + limit + '. Прочитайте или задайте связанный параметр.');
  }

  function makePlan(values, catalog, context = {}) {
    const effective = { ...context, ...values };
    return Object.entries(values).map(([id, input]) => {
      const p = catalog.get(id);
      if (!p || p.rw !== 'RW' || p.rwMode === 0) throw new Error('Недоступный для записи параметр ' + id);
      const value = numeric(input);
      const scaled = value / p.sc;
      const raw = Math.round(scaled);
      const signed = p.dt === 'int16';
      const min = resolveLimit(p.min, effective, catalog);
      const max = resolveLimit(p.max, effective, catalog);
      if (!Number.isFinite(scaled) || p.sc <= 0 || value < min || value > max
          || raw < (signed ? -32768 : 0) || raw > (signed ? 32767 : 65535)
          || Math.abs(scaled - raw) > 1e-7) {
        throw new Error(id + ' · допустимо ' + min + '…' + max + ', шаг ' + p.sc);
      }
      const option = p.en.split(';').find(entry => entry.split('=')[0] === String(value));
      if (p.dt === 'enum16' && !option) throw new Error('Неизвестный вариант ' + id);
      return { id, value: String(value), raw, label: option ? option.slice(option.indexOf('=') + 1) : '' };
    });
  }

  function applyPlan(plan, values, explicit) {
    for (const row of plan) {
      values[row.id] = row.value;
      explicit.add(row.id);
    }
  }

  function profileRows(catalog, values, explicit) {
    const byId = new Map(catalog.map(p => [p.id, p]));
    const selected = Object.fromEntries([...explicit].sort().map(id => [id, values[id]]));
    return makePlan(selected, byId, values).map(row => row.id + ' = ' + row.raw);
  }

  function readRaw(parameter, word) {
    if (!Number.isInteger(word) || word < 0 || word > 65535) throw new Error('Некорректный ответ FC03 для ' + parameter.id);
    return parameter.dt === 'int16' && word >= 0x8000 ? word - 0x10000 : word;
  }

  function importPlan(rawSet, catalog, context) {
    const settings = Object.create(null);
    for (const [id, raw] of Object.entries(rawSet)) {
      const p = catalog.get(id);
      if (!p || !Number.isInteger(raw)) throw new Error('Неизвестная или некорректная уставка ' + id);
      settings[id] = Number((raw * p.sc).toFixed(3));
    }
    return makePlan(settings, catalog, context);
  }

  // Standalone browser fallback. The Windows app uses the native ODS reader.
  function browserProfileSet(text) {
    let section = '', model = '';
    const set = Object.create(null);
    for (const line of text.split(/\r?\n/).map(s => s.trim())) {
      if (!line || line.startsWith(';')) continue;
      if (line.startsWith('[') && line.endsWith(']')) { section = line.slice(1, -1).trim(); continue; }
      const pos = line.indexOf('=');
      if (pos < 0) continue;
      const key = line.slice(0, pos).trim(), value = line.slice(pos + 1).trim();
      if (section === 'General' && key === 'Model') model = value;
      if (section === 'Parameter Set') {
        if (Object.hasOwn(set, key) || !/^[+-]?\d+$/.test(value)) throw new Error('Некорректная строка ' + key);
        set[key] = Number(value);
      }
    }
    if (model !== 'ПЧ ONI A650M') throw new Error('Ожидается профиль ПЧ ONI A650M.');
    return set;
  }

  function validGatewayIp(ip) {
    if (typeof ip !== 'string' || !ip.length) return false;
    if (ip.includes(':')) {
      if (!/^[a-fA-F0-9:.]+$/.test(ip)) return false;
      try { return !!new URL('http://[' + ip + ']/').hostname; } catch (_) { return false; }
    }
    const parts = ip.split('.');
    return parts.length === 4 && parts.every(part => /^(0|[1-9]\d{0,2})$/.test(part) && Number(part) <= 255);
  }

  function profileText(model, connection, confirmed, catalog, values, explicit, readings) {
    const { portName, slaveId, baudRate, frameFormat, simulated } = connection;
    const frame = /^1-(8)-([NEO])-([12])$/.exec(frameFormat);
    const ethernet = connection.mode === 'ethernet';
    if (!Number.isInteger(slaveId) || slaveId < 1 || slaveId > 247) throw new Error('Проверьте адрес ПЧ.');
    if (!ethernet && (!frame
        || !Number.isInteger(baudRate) || baudRate <= 0 || typeof portName !== 'string'
        || /[\r\n]/.test(portName) || (confirmed && !simulated && !portName.trim()))) throw new Error('Проверьте параметры подключения перед экспортом.');
    const lines = ['\uFEFF; Файл конфигурации ONI Drive Studio', '[General]', 'Model = ПЧ ONI ' + model,
      'Address = ' + slaveId];
    if (ethernet) {
      if (!validGatewayIp(connection.ip) || !Number.isInteger(connection.port)
          || connection.port < 1 || connection.port > 65535) throw new Error('Проверьте IP-адрес и TCP-порт шлюза.');
      lines.push('Transport = RTU over Ethernet', 'IP = ' + connection.ip, 'TcpPort = ' + connection.port);
    } else {
      if (portName) lines.push('Port = ' + portName);
      lines.push('BaudRate = ' + baudRate, 'FrameFormat = ' + frame.slice(1).join(''), 'Transport = ' + (simulated ? 'Simulator' : 'RS-485'));
    }
    lines.push('ConnectionSource = ' + (confirmed ? 'Confirmed' : 'Selected'),
      '', '[Parameter Set]', ...profileRows(catalog, values, explicit), '', '[Parameter Read]');
    for (const p of catalog) {
      if (readings.has(p.id)) lines.push(p.id + ' = ' + readings.get(p.id));
    }
    lines.push('', '[Check Parameter]', '');
    return lines.join('\r\n');
  }

  const api = { pumpSettings, pidSettings, makePlan, applyPlan, profileRows, readRaw, profileText, importPlan, browserProfileSet, validGatewayIp };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.WizardProfile = api;
})(typeof window !== 'undefined' ? window : globalThis);
