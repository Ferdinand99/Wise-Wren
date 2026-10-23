# Wise Wren – glossary (Chinese (Simplified), `zh-CN`)

Draft: not yet reviewed by native speakers.

Wren's own strings are written from English with these words. Core strings shared with Vigilant Vole come from
vole-kit and follow [Vigilant Vole's glossary](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/locales/glossary/zh-CN.md).

## Style

- 简体中文，使用中国大陆术语；中性、简洁，标签中不用“你/您”，句子中尽量省略主语，必要时用“你”。
- 全角标点：，。：；？！；引号用 “ ” 和 ‘ ’；省略号 `…` 保留；破折号 —。
- 中文与拉丁字母、数字之间不加空格（Apple 风格）；占位符与中文相邻也不加空格，除非为分隔点“ · ”。
- 保持原文：Docker、Compose、Unraid、`agent.toml`、`WW_SETUP_TOKEN`、`WW_PUBLIC_URL`、`docker run`、JSON，以及其他产品名、协议和技术术语。
- 无复数形式，`{n}` 后接量词：台（服务器）、个（探针、令牌、容器）、次（更新、回滚）、条（通知）、位（用户）。

## Terms

| English | 简体中文 | Note |
|---|---|---|
| update (noun) | 更新 |  |
| update (verb) | 更新 |  |
| target | 目标 | a container or Compose stack Wren updates; VV: “目标” |
| agent | 探针 | VV: “远程探针”; avoids 代理 (proxy), as in server-monitoring tools |
| server | 服务器 | count with 台 |
| rollback / roll back | 回滚 |  |
| snapshot | 快照 |  |
| maintenance window | 维护窗口 | VV |
| notification channel | 通知渠道 | VV |
| token / agent token | 令牌 / 探针令牌 | VV: “API 令牌” |
| revoke | 撤销 | kit: “撤销” |
| online / offline | 在线 / 离线 | VV: “在线”; kit: “离线” |
| waiting for the agent | 等待探针连接 | VV: “等待首次连接” |
| overview | 概览 |  |
| host name | 主机名 |  |
| operating system | 操作系统 | as Kestrel |
| Docker / Compose / Unraid | Docker / Compose / Unraid | unchanged; product names; “Docker套接字” |
| auto window | 自动窗口 | per-server window in which automatic updates may run |
| scheduled update | 计划的更新 | an update approved to run at a chosen time |
| monitor (Vigilant Vole) | 监控项 | VV: “监控项” |
