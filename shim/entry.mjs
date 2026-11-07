// preload bundle 入口：先装 <webview> 替身（renderer 创建 webview 前必须就位），再运行 cindy 的 preload.ts
// （暴露 window.electronAPI），最后挂冒烟检查。
import { installWebviewShim } from './webview.mjs';
import { installDragRegionShim } from './drag-region.mjs';
import '__CINDY_PRELOAD__';
import { installRendererSmoke } from './smoke.mjs';

installWebviewShim();
installDragRegionShim();
installRendererSmoke();
