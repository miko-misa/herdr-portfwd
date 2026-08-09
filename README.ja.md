# herdr-portfwd

[![CI](https://github.com/miko-misa/herdr-portfwd/actions/workflows/ci.yml/badge.svg)](https://github.com/miko-misa/herdr-portfwd/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

リモートホストの dev サーバーを手元の `localhost` に持ってきて、その転送の
開始・停止・管理を [Herdr](https://herdr.dev) の中だけで完結させるプラグインです。

*[English README](README.md)*

ビルド機の上でエージェントが `npm run dev` を実行し、`http://localhost:5173`
と表示する。それを Ctrl+クリックする。ポートが転送され、**同じポート番号のまま**、
あなたが実際に座っているマシンのブラウザでページが開きます。

## `ssh -L` と何が違うか

**どのポートがどのエージェントのものか分かります。** 実際のホストで待ち受けている
ポートの大半はインフラ — トンネルのデーモン、VM マネージャ、SSH の多重化ソケット
です。`herdr-portfwd` は各リスナーのプロセス系譜を Herdr ペインの `shell_pid` まで
遡るので、提示される一覧はこうなります:

```
mini: scholion   claude   :5173   node (vite)
mini: api        codex    :8787   workerd
```

こうはなりません:

```
127.0.0.1:20241  cloudflared    127.0.0.1:49313  limactl    *:53  limactl
```

ペインに帰属しないポートは、既定では提示されません。

**勝手に番号を振り直しません。** ローカル側のポートはリモートと同じ番号にします。
表示された URL がそのまま使えることに価値があるからです。番号を変えると Vite の
HMR、OAuth の `redirect_uri`、アプリが吐く絶対 URL が壊れます。衝突したときは
はっきり報告して選ばせます。黙って別のポートに逃がすことはしません。

**裏で勝手に転送しません。** 検出したポートは Herdr の通知で*提示*し、承認されて
から張ります。全自動の転送も用意していますが、ワークスペース単位のオプトインです。

## 必要なもの

- 両端に Herdr **0.8.0** 以降
- macOS または Linux（x86_64 / aarch64）
- 各ホストへの非対話な鍵認証:
  `ssh -o BatchMode=yes <host> true` がプロンプトなしで通ること
- ポート検出のため、リモート側に `lsof`（macOS）または `ss`（Linux）

## インストール

```bash
herdr plugin install miko-misa/herdr-portfwd
herdr server reload-config
```

`~/.local/bin/herdr-portfwd` に CLI も張られます。

ホスト定義は、次のうち**最初に見つかった**ファイルから読みます:

1. `~/.config/herdr-portfwd/hosts.toml`
2. `~/.config/herdr-mirror/hosts.toml`

書式は同じなので、[herdr-mirror](https://github.com/nikok6/herdr-mirror) を
使っているなら追加の設定は不要です。両方のファイルがある場合は herdr-portfwd 側が
優先され、herdr-mirror 側は完全に無視されます——マージはしません。
`herdr-portfwd status` が、読み込んだファイルと、それに隠された（`shadowed`）
ファイルを表示するので、ホストが消えたように見えるときは 1 行で分かります。

自分で書く場合は `~/.config/herdr-portfwd/hosts.toml` を作ってください:

```toml
[hosts.buildbox]
target = "buildbox"   # ssh が受け付ける形式なら何でも: alias, user@host, ssh://host:2222
prefix = "build"      # ミラーされたワークスペースの "build: " 接頭辞と対応
```

## どちらのモードで使っているか

Herdr はサーバーを接続のどちら側にも置けて、それがプラグインの実行場所を決めます。
`herdr-portfwd` は両方に対応しますが、設定方法が違います。

### herdr-mirror と併用（サーバーはローカル）

追加作業はありません。ローカルにプラグインを入れ、ローカルの
`~/.config/herdr/config.toml` にキーを割り当てれば終わりです。

### `herdr --remote`（サーバーはリモート）

`--remote` ではローカルの Herdr は薄いクライアントで、サーバーも、したがって
すべてのプラグインもリモートホストで動きます。リモートのプラグインは、あなたの
ラップトップにポートを開くことができません。そこで、プラグインは「何を転送するか」
を宣言するだけにして、小さなローカルエージェントが実際の転送を行います。

`herdr --remote` を直接叩く代わりに、ラッパー経由でセッションを開始してください:

```bash
herdr-portfwd attach buildbox     # ローカルエージェントを起動してから herdr --remote を exec
```

さらに 2 点、違いがあります:

- プラグインは**リモートホスト側に**インストールします（そちらで `herdr plugin install ...`）
- キーバインドは**リモートの** `config.toml` に書きます。Herdr はリモートサーバーへ
  送るプロファイルから `[[keys.command]]` を——`type = "plugin_action"` も含めて——
  すべて取り除くため、ローカルの設定は届きません。

## 使い方

**URL を Ctrl+クリック。** ペイン内の `http://localhost:PORT` や
`http://127.0.0.1:PORT` がプラグインに渡り、転送してからローカルで開きます。
Cmd+クリックではなく Ctrl+クリックなのは、端末のマウスレポートが Cmd+クリックを
ただのクリックと区別できないためで、これは全プラットフォーム共通です。

**一覧から選ぶ。** `scan` アクションがホストごとの帰属済みリスナーを表示し、
選んだものを転送します。

**動いているものを管理する。** `list` アクションが、生きている全転送と、その
持ち主のペイン、停止キーをオーバーレイで表示します。

### アクション

| アクション | 動作 |
| --- | --- |
| `portfwd.scan` | 帰属済みのリモートリスナーを一覧し、選んだものを転送 |
| `portfwd.add` | ポートを転送（ポート未指定ならピッカーを開く） |
| `portfwd.list` | 生きている転送のオーバーレイ（停止キー付き） |
| `portfwd.stop` | 選んだ転送を停止 |
| `portfwd.stop-all` | 現在のホストの転送をすべて停止 |
| `portfwd.open-url` | リンクハンドラの飛び先。転送してからローカルで開く |

### キーバインド

既定のキーは割り当てていません。`config.toml` で設定してください。`herdr --remote`
なら**リモート側**、herdr-mirror ならローカル側です:

```toml
[[keys.command]]
key = "prefix+p"
type = "plugin_action"
command = "portfwd.scan"
description = "forward a remote port"

[[keys.command]]
key = "prefix+shift+p"
type = "plugin_action"
command = "portfwd.list"
description = "manage port forwards"
```

そのあと `herdr server reload-config` を実行します。

## 設定

`~/.config/herdr-portfwd/config.toml`:

```toml
# 新しいリスナーを探す間隔。0 で検出そのものを無効化。
poll_seconds = 5

# 検出したポートを Herdr の通知で提示する。既定: true
offer_new_listeners = true

# Herdr のペインに帰属するリスナーだけを提示する。既定: true
require_pane_attribution = true

# オプトインの自動転送。ワークスペース単位で照合。既定では空。
[[auto]]
workspace = "scholion"
ports = [5173, 8787]
```

## うまく動かないとき

### まず `status`

```bash
herdr-portfwd status
```

何かがすでにおかしいときに実行するコマンドなので、失敗せずに「報告」します。
読み込んだ `hosts.toml` とそれが隠しているファイル、状態ファイルとリクエスト
ファイルの場所、この呼び出しがどのホストに解決されたか**とその理由**、ホストごとの
コントロールマスターの有無、記録されている転送、ローカルエージェント待ちの
リクエスト数を表示します。grep したいときは `--json` を付けてください。

「待ち」に数えるのは、まだ誰も処理していないものだけです。ローカルエージェントは
どこまで処理したかを `requests.json` の隣の `requests.acked` に記録し、キューを
読むたびにその印を折り込むので、すでに適用済みのリクエストは数から外れます——
`herdr --remote` で `status` が動くリモートホスト側でも同じです。キューがいつまでも
減らないなら、エージェントが動いていないか、読んでいるリクエストファイルの隣に
書き込めていません。

Herdr が実行したコマンドの記録は `herdr plugin log list --plugin portfwd` に
あります。サーバーが動いているマシン側——`herdr --remote` ならリモート側——です。

### `--host`: 選ばれたのと違うホストを対象にする

すべてのコマンドで使え、他のどの手掛かりよりも優先されます（呼び出し元
ワークスペースのミラー接頭辞、`default_host`、ホストが 1 つだけという規則、
いずれよりも強い）:

```bash
herdr-portfwd list --host buildbox
```

シェルからは `HERDR_PORTFWD_HOST` でも同じことができます。`status` が違うホストを
指している、あるいは何も選べていないときに使ってください。*"cannot tell which host
to use"* は、複数のホストが設定されていて、呼び出しからはどれとも決められなかった、
という意味です。

### `HERDR_PORTFWD_MODE`: どちら側にいるかの判定を上書きする

あなたのマシンにポートを開けるのは、そのマシン上のプロセスだけです。そのため
プラグインは、自分が SSH 接続のどちら側で動いているかを知る必要があります。
Herdr が渡す情報にそれを示すものはないので、次の順で推定します: ミラーされた
ワークスペースの接頭辞、ここで生きているコントロールマスター、このサーバーに
ミラーされたペイン、いずれでもなければリモート側とみなしてローカルエージェント宛の
リクエストを記録する。`status` は結論と理由の両方を、たとえば
`local (live control master)` のように表示します。

その推定が外れているときは、明示してください:

```bash
HERDR_PORTFWD_MODE=local herdr-portfwd add 5173    # ここでポートを開く
HERDR_PORTFWD_MODE=remote herdr-portfwd add 5173   # ローカルエージェント宛にリクエストを記録
```

この値は上のどの規則よりも優先され、`status` は `local (HERDR_PORTFWD_MODE)` と
表示します。`local` / `remote` 以外の値は致命的エラーにはせず無視します。export
せず、コマンドごとに指定してください。あなたのマシンでないところで `local` に
すると誰も到達できないポートを開き、自分のマシンで `remote` にすると、
`herdr-portfwd attach <host>` でエージェントを起動しない限り、誰も適用しない
リクエストが残ります。

## セキュリティ

このプラグインはあなたの権限・鍵・ssh-agent で SSH を実行し、手元では loopback
にしか bind しません。こちら側で起動する外部コマンドはすべて argv 配列で、
文字列をシェルに渡すことはありません。`ssh` はリモートコマンドを向こう側の
ログインシェルに渡すため、あちらで動くわずかなものにも同じ規則を反対側から
適用しています。リクエストファイルを探す断片と、エージェントの進捗を記録する
断片は固定テキストで、埋め込まれうるのは数値だけです。`herdr` の呼び出しは
単語ごとにクォートし、シェル解釈をちょうど 1 回だけ通るようにしています。
報告手順は [SECURITY.md](SECURITY.md) を参照してください。

## 開発

```bash
just check      # fmt + clippy + テスト
just test
cargo build --release
herdr plugin link .          # この作業ツリーをリンク
herdr server reload-config
```

テストは実際の `ssh`・実際の `herdr`・ネットワークを一切叩きません（どちらも
トレイト越しのフェイクに差し替わります）。設計とその不変条件は
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)、プルリクエスト前に
[CONTRIBUTING.md](CONTRIBUTING.md) を参照してください。

## ライセンス

MIT — [LICENSE](LICENSE) を参照。
