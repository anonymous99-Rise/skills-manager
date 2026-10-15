# 跟上游同步的流程(内部文档)

## 分支模型

- `main`(GitHub fork / 公司 Gitea)= 上游 `main` + 我们的 delta(internal 改造)
- `upstream/main` = xingkongliang/skills-manager 的主线(只读)

已配置的 remote:
- `origin` → github.com/anonymous99-Rise/skills-manager(推送:GitHub + Gitea 两处同步)
- `upstream` → github.com/xingkongliang/skills-manager(只取不推)

## 日常同步(每次上游发新版时)

```bash
git fetch upstream
git merge upstream/main        # 用 merge,不要 rebase(历史已共享到 Gitea)
```

**冲突只可能出现在这些文件**(我们的 delta 全部清单,其余都是纯新增文件,不会冲突):

| 文件 | 我们改了什么 | 冲突处理原则 |
|---|---|---|
| `src-tauri/src/core/error.rs` | `From<tauri::Error>` 加了 `#[cfg(feature = "desktop")]` | 上游若动了该 impl,保留门控 |
| `src-tauri/src/core/git_credentials.rs` | `load_credential` 开头加了 env 兜底 | 上游若重写该函数,把 env 段落重新放回函数体最前 |
| `src-tauri/Cargo.toml` | `[features] desktop = []` | 上游若加了自己的 features,合并两段 |
| `src-tauri/tauri.conf.json` | updater pubkey(我们自己的密钥) | **永远保留我们的 pubkey** |
| `.github/workflows/release.yml` | Apple 签名可选化 | 上游动了签名段落时,重放 MACOS_SIGNING 门控 |
| `src/lib/tauri.ts` + 10 个视图/组件 | import 行换成 `../lib/transport` | 上游若改了同文件的其它逻辑,只保留我们的 import 差异 |
| `src/i18n/index.ts` | 兜底语言 `return "zh"` | 保留 zh |
| `.gitignore` | 追加 server 相关忽略 | 纯追加 |

`server/`、`src/lib/transport.ts`、`server/src/**` 为纯新增,合并零冲突。

## 合并后必做

1. `npm install && npm run build` — 前端 TS 编译通过
2. WSL 重建服务端二进制(deploy.md 第 1 步)
3. `server/verify.sh` 对生产或本地跑一轮(fail 必须为 0)
4. 若上游改了 `commands/*.rs` 里我们移植过的逻辑(ops.rs/backup.rs/tools.rs 的对应实现),同步移植后重新部署
5. 提交合并结果,推送 origin + Gitea

## 缩小 delta 的策略

小的 enabling 改动(feature 门控、签名可选化、env 凭据兜底)值得给上游提 PR —— 被合并一项,上面清单就少一项,以后合并就少一处冲突。架构级改动(服务端模式)先开 issue 讨论再动。

## 发版时的 tag 坑(重要)

`git fetch upstream` 会把上游的 `v*` tag 一并拉到本地。上游发了 `v1.42.0` 之后,本地 `v1.42.0` 指向的是**上游的 commit**——此时直接 `git push origin v1.42.0` 会把纯上游代码(不含我们的 delta)发成我们的版本。发版前必须:

```bash
git rev-parse v1.42.0^{commit}   # 确认指向;若指向上游 commit,先 git tag -d 再重打到我们的 commit
```

推完用 `git ls-remote origin "refs/tags/v1.42.0^{}` " 复核两处远端都指向 fork 的 commit。
