// maker-core SessionStorage 的 node:sqlite 实现（参照 cindy-headless session-storage.ts）。
// 只存 maker-core 自己的 session 元数据（sdkSessionId 等 resume 所需字段），
// 产品层会话列表由 B 的 local-db 管，两者互不依赖。
import { DatabaseSync } from 'node:sqlite';
import { mkdirSync } from 'node:fs';
import path from 'node:path';

export class SqliteSessionStorage {
  constructor(filePath) {
    mkdirSync(path.dirname(filePath), { recursive: true });
    this.db = new DatabaseSync(filePath);
    this.db.exec('PRAGMA journal_mode = WAL');
    this.db.exec('PRAGMA busy_timeout = 5000');
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS maker_sessions (
        id TEXT PRIMARY KEY,
        data TEXT NOT NULL,
        updated_at INTEGER NOT NULL
      )
    `);
  }

  #read(id) {
    const row = this.db.prepare('SELECT data FROM maker_sessions WHERE id = ?').get(id);
    return row ? JSON.parse(row.data) : null;
  }

  #write(row) {
    this.db.prepare(
      'INSERT INTO maker_sessions (id, data, updated_at) VALUES (?, ?, ?) ' +
      'ON CONFLICT(id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at',
    ).run(row.id, JSON.stringify(row), row.updatedAt);
  }

  async create(meta) {
    const now = Date.now();
    // 同 id 重建（重启后 resume）时保留原 createdAt。
    const prev = this.#read(meta.id);
    const row = { ...meta, createdAt: prev?.createdAt ?? now, updatedAt: now };
    this.#write(row);
    return row;
  }

  async get(id) {
    return this.#read(id);
  }

  async list() {
    return this.db.prepare('SELECT data FROM maker_sessions ORDER BY updated_at DESC').all()
      .map((row) => JSON.parse(row.data));
  }

  async update(id, patch) {
    const current = this.#read(id);
    if (!current) throw new Error(`unknown session ${id}`);
    const row = { ...current, ...patch, id: current.id, updatedAt: Date.now() };
    this.#write(row);
    return row;
  }

  async compareAndClearSdkSessionId(id, expectedSdkSessionId) {
    this.db.exec('BEGIN IMMEDIATE');
    try {
      const current = this.#read(id);
      if (!current || current.sdkSessionId !== expectedSdkSessionId) {
        this.db.exec('COMMIT');
        return false;
      }
      const { sdkSessionId: _drop, ...rest } = current;
      this.#write({ ...rest, updatedAt: Date.now() });
      this.db.exec('COMMIT');
      return true;
    } catch (err) {
      this.db.exec('ROLLBACK');
      throw err;
    }
  }

  async delete(id) {
    this.db.prepare('DELETE FROM maker_sessions WHERE id = ?').run(id);
  }

  close() {
    this.db.close();
  }
}
