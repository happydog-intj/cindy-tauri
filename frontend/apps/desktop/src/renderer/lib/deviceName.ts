import { deviceDisplayName as displayName, type DeviceNameSource } from '@cindy/maker-shared/device-list';
// The renderer initializes this singleton; display helpers do not initialize React bindings.
import i18n from 'i18next';
// 只用这一个字符串，不导入整份英文 JSON（约 1 MB 会被拖进首屏）；值与 en/common.json 的 bots.cloud.unknownDevice 一致。
const UNKNOWN_DEVICE_EN = 'another computer';

export function deviceDisplayName(deviceId: string, name?: string | null, devices: readonly DeviceNameSource[] = []): string {
  const unnamed = i18n.isInitialized
    ? i18n.t('bots.cloud.unknownDevice', { defaultValue: UNKNOWN_DEVICE_EN })
    : UNKNOWN_DEVICE_EN;
  return displayName(deviceId, name, unnamed, devices);
}
