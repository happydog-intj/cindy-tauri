// 设置页通道入口：外观 / 快捷键 / 主题 / 更新通道 / 终端 / 外设（app-settings），
// maker 侧开关（maker-settings），语音输入数据（voice-input-data），压缩阈值 / Pi 内核 / 用量 / 模型链（misc-settings）。
// 设置统一存放在 <dataDir>/settings/*.json；需要在 local-db / maker-ipc 之前注册，后注册者覆盖。
import { mkdirSync } from 'node:fs';
import path from 'node:path';
import { registerAppSettingsIpc } from './settings/app-settings.mjs';
import { registerMakerSettingsIpc } from './settings/maker-settings.mjs';
import { registerMiscSettingsIpc } from './settings/misc-settings.mjs';
import { registerVoiceInputDataIpc } from './settings/voice-input-data.mjs';

export function registerSettingsIpc({ dataDir, piStatus }) {
  const settingsDir = path.join(dataDir, 'settings');
  mkdirSync(settingsDir, { recursive: true });
  const app = registerAppSettingsIpc({ settingsDir });
  registerMakerSettingsIpc({ settingsDir });
  registerMiscSettingsIpc({ settingsDir, piStatus });
  registerVoiceInputDataIpc({ settingsDir });
  return { readAutoRelaunchOnIdle: app.readAutoRelaunchOnIdle };
}
