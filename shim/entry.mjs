// preload bundle 入口：先运行 cindy 的 preload.ts（暴露 window.electronAPI），再挂冒烟检查。
import '__CINDY_PRELOAD__';
import { installRendererSmoke } from './smoke.mjs';

installRendererSmoke();
