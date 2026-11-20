// A selection edit revokes the current session synchronously. Native lifecycle
// calls run in order so a delayed disconnect cannot close a newer connection.
class DriveSessionGate {
  constructor(invoke) {
    this.invoke = invoke;
    this.active = null;
    this.revision = 0;
    this.pending = Promise.resolve();
  }

  transition(command, args = {}) {
    const revision = ++this.revision;
    this.active = null;
    const operation = this.pending.then(async () => {
      if (revision !== this.revision) return null;
      const status = await this.invoke(command, args);
      if (revision !== this.revision || command === 'disconnect_serial') return null;
      if (!status || !Number.isSafeInteger(status.session_id) || (!status.simulated && !status.connected)) {
        throw new Error('Подключение к ПЧ не подтверждено.');
      }
      this.active = status;
      return status;
    });
    this.pending = operation.catch(() => {});
    return operation;
  }

  async request(session, command, args = {}) {
    if (!session || session !== this.active) throw new Error('Подтвердите подключение к выбранному ПЧ.');
    const result = await this.invoke(command, { ...args, sessionId: session.session_id });
    if (session !== this.active) throw new Error('Сеанс подключения изменился.');
    return result;
  }
}

if (typeof module !== 'undefined' && module.exports) module.exports = DriveSessionGate;
