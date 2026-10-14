# CLAUDE.md — skills-manager 内部 fork 操作指南

本仓库 = 上游 xingkongliang/skills-manager 的 fork + 内部增量(Web 服务端 / Gitea 集成)。
架构与背景见 `server/UPSTREAM_SYNC.md` 和 `server/deploy.md`。

## Remotes 与推送纪律

- `origin` = github.com/anonymous99-Rise/skills-manager(公开 fork)
- 公司 Gitea 镜像 = git.nxwysoft.com/zhangz/skills-manager(推送用 token 内联 URL;token 在 `server/.env` 的 `SM_GIT_CRED_GIT_NXWYSOFT_COM`,格式 `statistical-rankingBot:<token>`,或直接从 shell 历史找)
- `upstream` = xingkongliang/skills-manager(**只 fetch,永不 push**)
- **每次提交后必须推两处**:origin 和 Gitea

## 用户说「同步上游 / merge 上游 / rebase 上游」时

严格按 `server/UPSTREAM_SYNC.md` 执行,要点:

1. `git fetch upstream`
2. `git merge upstream/main` — **禁止 rebase**(历史已推送到 Gitea 共享,rebase 会反复改写;即使用户口头说 rebase,也要先说明这一点再执行 merge)
3. 冲突只可能出现在 UPSTREAM_SYNC.md 的清单文件里,处理原则:
   - `tauri.conf.json` → **永远保留我们的 updater pubkey**
   - `error.rs` → 保留 `#[cfg(feature = "desktop")]` 门控
   - `git_credentials.rs` → 把 env 兜底段重新放回 `load_credential` 最前
   - `release.yml` → 重放 `MACOS_SIGNING` 门控
   - 前端 import 行 → 保留 `../lib/transport` 引用
   - 其余(`server/**`、`transport.ts`)为纯新增,不会冲突
4. 合并后必做(全绿才算完成):
   - `npm install && npm run build`
   - WSL musl 重建服务端:`wsl -d kali-linux -- bash -c "rsync -a --exclude .git --exclude 'src-tauri/target' --exclude node_modules --exclude dist /mnt/d/devlops/skills-manager/ ~/sm/ && source \$HOME/.cargo/env && cd ~/sm/server && cargo build --target x86_64-unknown-linux-musl --release && cp target/x86_64-unknown-linux-musl/release/skills-manager-server /mnt/d/devlops/skills-manager/server/"`
   - 若上游改了 `commands/*.rs` 中被 `server/src/ops.rs`、`backup.rs`、`tools.rs` 移植过的逻辑 → 同步移植
   - `server/verify.sh` 跑一轮,fail 必须为 0
5. 提交(带 `Co-Authored-By`)并推 origin + Gitea

## 用户说「部署 / 更新服务器」时

按 `server/deploy.md`:
1. 二进制构建(上面第 4 步的 WSL 命令)
2. 前端 `npm run build` → `dist` 复制为 `server/public`
3. `scp server/{skills-manager-server,public,Dockerfile,docker-compose.yml,.env} root@36.1.40.14:/opt/skills-manager/`
4. 远端 `chmod +x skills-manager-server && docker compose up -d --build`(**必须 --build**,改宿主机文件不会自动进镜像)
5. `curl http://36.1.40.14:18080/api/health` 应返回 ok

## 用户说「发版 / release」时

- 推 `v*` tag 到 GitHub 即自动触发构建签名发包(也支持 Actions 页手动 Run workflow)
- 构建全部通过后流水线**自动 Publish**(最后一个 Publish release job 负责把验证过的草稿转正),无需手动操作
- 更新器密钥对在 `D:\devlops\tauri-updater-keys\`(密码同步存在 GitHub secret;公钥在 tauri.conf.json,三者必须匹配)

## 已知坑(踩过的,别再踩)

- 杀本地服务端:`pkill -x skills-manager-server` **杀不掉**(comm 截断 15 字符),要用 `ss -tlnp | grep :18080` 取 PID 再 `kill -9`
- WSL 里 curl 默认走宿主机代理(127.0.0.1:7897),本地测试加 `--noproxy '*'` 或 `export no_proxy=127.0.0.1`
- 服务器 clash 在 127.0.0.1:7890(容器已是 host 网络直达);GitHub API 走 clash 出口常被限流,`check_app_update` 已做无更新兜底
- cargo 构建命令里不要用 grep 过滤错误后只看 tail —— 要看完整输出;`cp` 必须在构建成功之后
- secrets 只存在于 `server/.env`(已 gitignore),仓库里任何文件不得出现 token/密码明文
