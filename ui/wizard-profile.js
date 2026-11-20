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

  function makePlan(values, catalog) {
    return Object.entries(values).map(([id, input]) => {
      const parameter = catalog.get(id);
      const value = Number(input);
      if (!parameter || parameter.rw !== 'RW') throw new Error('Недоступный параметр ' + id);
      const raw = value / parameter.sc;
      if (input == null || String(input).trim() === '' || !Number.isFinite(raw)
          || value < Number(parameter.min) || value > Number(parameter.max)
          || Math.abs(raw - Math.round(raw)) > 1e-7) {
        throw new Error(id + ' · допустимо ' + parameter.min + '…' + parameter.max + ', шаг ' + parameter.sc);
      }
      const option = parameter.en.split(';').find(entry => entry.split('=')[0] === String(value));
      if (parameter.dt === 'enum16' && !option) throw new Error('Неизвестный вариант ' + id);
      return { id, value: String(value), raw: Math.round(raw), label: option ? option.slice(option.indexOf('=') + 1) : '' };
    });
  }

  function applyPlan(plan, values, explicit) {
    for (const row of plan) {
      values[row.id] = row.value;
      explicit.add(row.id);
    }
  }

  function profileRows(catalog, values, explicit) {
    const rows = [];
    for (const p of catalog) {
      const value = values[p.id];
      if (value === '----' || (!explicit.has(p.id) && String(value) === String(p.def))) continue;
      const raw = Number(value) / p.sc;
      if (value == null || String(value).trim() === '' || !Number.isFinite(raw)) {
        throw new Error('Проверьте значение ' + p.id + ' перед экспортом.');
      }
      rows.push(p.id + ' = ' + Math.round(raw));
    }
    return rows;
  }

  const api = { pumpSettings, pidSettings, makePlan, applyPlan, profileRows };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.WizardProfile = api;
})(typeof window !== 'undefined' ? window : globalThis);
