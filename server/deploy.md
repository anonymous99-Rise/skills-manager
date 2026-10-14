# skills-manager-server 部署手册

## 架构

```
浏览器 ──▶ http://36.1.40.14:8080  (docker: alpine + musl 静态二进制)
              ├─ /            前端静态文件(SPA,与桌面版同一份 React 代码)
              ├─ /api/invoke/:cmd   invoke-over-HTTP(与桌面 Tauri IPC 同名同参)
              └─ /api/auth/gitea/*  Gitea OAuth2 登录
                     │
                     ▼
          /data/.skills-manager/   团队中心技能库(SQLite + skills git 库)
                     │ git 自动备份
                     ▼
          git.nxwysoft.com/zhangz/skills-library (私有)
```

桌面端继续完全照旧(本地库 + GitHub/skills.sh 上游);Web 端 = 团队统一库。

## 本地构建(Windows)

```bash
# 1. 服务端二进制(WSL kali, musl 静态)
wsl -d kali-linux -- bash -c "rsync -a --delete --exclude .git --exclude 'src-tauri/target' \
  --exclude node_modules --exclude dist /mnt/d/devlops/skills-manager/ ~/sm/ && \
  source \$HOME/.cargo/env && cd ~/sm/server && \
  cargo build --target x86_64-unknown-linux-musl --release && \
  cp target/x86_64-unknown-linux-musl/release/skills-manager-server /mnt/d/devlops/skills-manager/server/"

# 2. 前端
cd /d/devlops/skills-manager && npm run build
rm -rf server/public && cp -r dist server/public

# 3. 上传 + 起容器
scp -r server/{Dockerfile,docker-compose.yml,skills-manager-server,public} \
  root@36.1.40.14:/opt/skills-manager/
ssh root@36.1.40.14 "cd /opt/skills-manager && docker compose up -d --build"
```

## 服务器(/opt/skills-manager)

- `data/` 卷:SQLite 库 + skills git 库 + 会话密钥
- 环境变量见 docker-compose.yml:Gitea OAuth、SM_PUBLIC_URL、git 凭据
- git 凭据规则:`SM_GIT_CRED_<HOST非字母数字转_>` = `user:token`,优先于系统 keychain

## Gitea 侧(已配置)

- OAuth2 应用 `skills-manager-web`(bot 账号名下):
  client_id `488063a7-9f0c-44c9-90b9-3b282aea3ef3`
  redirect: `http://36.1.40.14:8080/api/auth/gitea/callback`
- 备份仓库 `zhangz/skills-library`(私有)
- 源码镜像 `zhangz/skills-manager`(公开)

## 云flare(可选,后续)

skills.dfyx.click 若继续走 Cloudflare Pages:在 `public/_redirects` 加
`/api/* http://36.1.40.14:8080/api/:splat 200`,Pages 部署即可同源代理;
Gitea OAuth 应用需追加 `https://skills.dfyx.click/api/auth/gitea/callback`,
并设置 `SM_PUBLIC_URL=https://skills.dfyx.click`。

## 已知边界(web 版)

- 桌面专属功能返回 501:本地文件夹导入、GitHub 设备流、项目/工作区、应用内更新
- 服务端无事件推送:安装进度以整块请求等待呈现
- 登录用户范围:默认 Gitea 全部用户,可用 SM_ALLOWED_USERS 收紧
