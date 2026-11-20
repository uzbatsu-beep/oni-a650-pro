function outputDirection(statusWord) {
  if (!Number.isInteger(statusWord)) return null;
  return (statusWord >> 1) & 1;
}

function describeFault(stateCode, faultCode, catalog) {
  if (Number.isInteger(faultCode) && faultCode > 0) {
    const key = 'Err' + String(faultCode).padStart(2, '0');
    const entry = catalog.find(fault => fault.code === key);
    return {
      active: true, color: 'danger', label: key + ' (2102H=' + faultCode + ')',
      detail: entry?.description || 'Код отсутствует в справочнике.',
      action: entry?.action
    };
  }
  if (stateCode === 6) {
    return { active: true, color: 'danger', label: 'Авария ПЧ', detail: '2100H = 6. Код аварии пока не подтверждён.' };
  }
  if (faultCode === 0) {
    return { active: false, color: 'success', label: '00 · Норма', detail: 'Текущих аварий нет (2102H = 0).' };
  }
  return { active: false, color: 'text-muted', label: 'Нет данных', detail: 'Нет актуального значения 2102H.' };
}

if (typeof module !== 'undefined' && module.exports) module.exports = { outputDirection, describeFault };
