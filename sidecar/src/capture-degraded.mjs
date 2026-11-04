// 截图、录屏、远程桌面的降级通道（P3 决定：这些能力在 Tauri 版不移植，见 DESIGN.md 9.7）。
// 只给 renderer 首屏和设置页会读的通道回 desktop 同形状的「关闭 / 不可用」结果，
// 会改变状态的入口统一报 UNSUPPORTED_CAPABILITY，renderer 按失败处理。
import { registerInvoke } from './ipc.mjs';

const unsupported = (what) => () => {
  throw new Error(`[UNSUPPORTED_CAPABILITY] ${what} is not available in the Tauri build`);
};

export function registerCaptureDegradedIpc() {
  // shared/remoteDesktop.ts DesktopLocalState；远程桌面依赖 device-link，未移植，始终关闭。
  registerInvoke('remote-desktop:state', (checkWindowsSupport) => {
    if (checkWindowsSupport !== undefined && typeof checkWindowsSupport !== 'boolean') {
      throw new Error('[INVALID_PARAMS] Invalid Windows support status request');
    }
    return {
      enabled: false,
      active: null,
      overlay: false,
      permissionGuide: false,
      ...(checkWindowsSupport === true ? { windowsSupport: 'unavailable' } : {}),
    };
  });
  // device-link RemoteDesktopPermissions；不做系统权限探测。
  registerInvoke('remote-desktop:permissions', () => ({ screenRecording: 'unknown', accessibility: 'unknown' }));
  registerInvoke('remote-desktop:enable', unsupported('remote desktop'));
  registerInvoke('remote-desktop:windows-support', unsupported('remote desktop'));
  registerInvoke('remote-desktop:open-permission', unsupported('remote desktop'));
  registerInvoke('remote-desktop:stop', () => undefined);
  registerInvoke('remote-desktop:dismiss-permission-guide', () => undefined);
  // 全局截图交给新任务：没有截图入口，永远没有待处理请求。
  registerInvoke('region-screenshot:compose-take', () => null);
}
