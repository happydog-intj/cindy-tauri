# cindy-maker-main

`maker-freeze` 用的上游较新版本（main）的 `maker-core` / `maker-shared` 源文件，只含 `translator.ts` 的依赖闭包（108 个文件，无 npm 依赖）。
pi-rs 的 maker 事件序列对齐这一版；渲染层和其它代码用 `vendor/cindy`（固定提交，见其 `.upstream-commit`）。
同步：`scripts/vendor-maker-main.sh <上游目录>`。
