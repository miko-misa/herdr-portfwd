# herdr-portfwd

[![CI](https://github.com/miko-misa/herdr-portfwd/actions/workflows/ci.yml/badge.svg)](https://github.com/miko-misa/herdr-portfwd/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

リモートホストの dev サーバーを手元の `localhost` に持ってきて、その転送の
開始・停止・管理を [Herdr](https://herdr.dev) の中だけで完結させるプラグインです。

*[English README](README.md)*

ビルド機の上でエージェントが `npm run dev` を実行し、`http://localhost:5173`
と表示する。それを Ctrl+クリックする。ポートが**同じ番号のまま**あなたが実際に
座っているマシンへ転送され、そのマシンのブラウザでページが開きます。herdr-mirror
なら即座に、`herdr --remote` ならローカルエージェントの次のポーリング（既定 5 秒
以内）で——`--remote` では、転送を実行できるのはあなたのマシンで動くこのツールの
片割れだけだからです。

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
表示された URL がそのまま使えることに価値があるからです。衝突したとき——手元の
別プロセスが握っている、あるいは別のリモートポートの転送が使っている——は、
何が握っているかを報告するだけで、何も変更しません。明示的に選ぶ手段は `add` の
`--local-port` です:

```bash
herdr-portfwd add 5173 --local-port 15173
```

番号を変える方法はこれだけです。Ctrl+クリックは決して振り直しませんし、ピッカーにも
振り直しの操作はありません。そして選んだ場合、5173 を埋め込んでいるものはすべて
壊れます——Vite の HMR、OAuth の `redirect_uri`、アプリが吐く絶対 URL。向こう側の
サーバーは自分が 5173 にいると思ったままだからです。

**裏で勝手に転送しません。** 新しく検出したポートは Herdr の通知になり、ポート番号・
プロセス・持ち主のペイン、そして転送するためのコマンド（`herdr-portfwd add 5173`）
を伝えます。**承認キーはありません**——Herdr の通知はトーストなので、「承認」とは
そのコマンドを自分で実行すること、あるいは `scan` の一覧から選ぶことです。全自動の
転送も用意していますが、ワークスペース単位のオプトインです。

## 必要なもの

あなたが座っているマシン側:

- Herdr **0.8.0** 以降
- macOS または Linux（x86_64 / aarch64）
- OpenSSH の `ssh`（ControlMaster 多重化、`-O forward` / `-O check`）。転送は
  すべて、このツールが自分で持つマスター上で行います
- 各ホストへの非対話な鍵認証:
  `ssh -o BatchMode=yes <host> true` がプロンプトなしで通ること
- `open`（macOS）または `xdg-open`（Linux）。ブラウザへの受け渡しは、ブラウザが
  あるこちら側で必ず行われます
- `herdr --remote` の場合は `herdr-portfwd` 自体。`attach` とローカルエージェント
  はローカルのコマンドです（後述の `herdr --remote` の項にある 2 台構成を参照）

各リモートホスト側:

- Herdr **0.8.0** 以降。`herdr --remote` ではこちらがサーバーで、プラグインの
  コマンドはすべてそこで動きます
- `lsof`（macOS）または `ss`（Linux）。検出はそのマシン上で
  `lsof -nP -iTCP -sTCP:LISTEN -F pcn` あるいは `ss -ltnpH` を実行します
- `ps`。帰属判定は `ps -eo pid=,ppid=` の親子マップを遡るので、これがないと
  どのリスナーもペインに結び付けられず、`scan` は何も提示できません
- POSIX のログインシェル。`ssh` はリモートコマンドをそれに渡します
- `herdr --remote` の場合は、プラグイン本体と、そのマシン自身の `hosts.toml`
  （後述）

`herdr plugin install` 自体には、インストールするマシンに `git`（Herdr が clone
します）と `bash`（マニフェストのビルド手順が `bash scripts/install.sh` です）、
それに取得したバイナリの検証用として `curl` と、`sha256sum` か `shasum` が必要です。

## インストール

```bash
herdr plugin install miko-misa/herdr-portfwd
herdr server reload-config
```

インストール処理は `~/.local/bin/herdr-portfwd` に CLI を置くことも**試みます**。
シェルから `herdr-portfwd <cmd>` を使えるようにし、キーバインドに安定したパスを
1 つ与えるためです。シンボリックリンクではなく**コピー**なのは、Herdr がビルド手順の
実行後にプラグインのチェックアウトを別の場所へ移動するからです。

このコピーは完全にベストエフォートで、インストール自体を失敗させることはありません。
`HOME` が未設定のとき、`~/.local/bin` を作れないか書き込めないとき、その名前が
すでにディレクトリとして存在するときは、理由を書いた 1 行を stderr に出して
スキップされ、インストール自体は成功します:

```
herdr-portfwd: could not install the CLI on PATH: cannot write to /home/you/.local/bin
```

スキップされても失うものは思ったより小さいはずです。アクションもペインも
Ctrl+クリックのハンドラも、プラグインのチェックアウト内のバイナリをパスで直接
実行するので、Herdr が起動するものはすべてそのまま動きます。無くなるのは自分で打つ
コマンドのほうです——`herdr-portfwd status`、提示の通知が実行を促してくる
`herdr-portfwd add <port>`、そして `herdr --remote` モードそのものである
`herdr-portfwd attach <host>`。手に入れるには、メッセージ自身が言うとおり、パスを
直して（たいていは `mkdir -p ~/.local/bin`）
`herdr plugin install miko-misa/herdr-portfwd` を再実行してください。コピー先は
常にこの 1 つのディレクトリなので、そこが `PATH` に入っているかも確認してください。

ホスト定義は、コマンドが動いているマシン上で、次のうち**最初に見つかった**ファイル
から読みます:

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

プラグインがあなたのマシンで動くので、すべて自分で完結できます。ローカルに
プラグインを入れ、ローカルの `~/.config/herdr/config.toml` にキーを割り当てて
リロードすれば終わりです。Ctrl+クリックは、転送とブラウザでの表示を一息で行います。

このモードで動かないものが 1 つあります。新しいリスナーを見つけて L2 の提示を出し、
`[[auto]]` を適用するポーリングはローカルエージェントの仕事で、そのエージェントを
起動するのは `herdr --remote` 側の `attach` だからです。herdr-mirror では、ポートは
Ctrl+クリック・`scan`・`add` で転送します。

### `herdr --remote`（サーバーはリモート）

`--remote` ではローカルの Herdr は薄いクライアントで、サーバーも、したがって
すべてのプラグインもリモートホストで動きます。リモートのプラグインは、あなたの
ラップトップにポートを開くことも、あなたのブラウザに触ることもできません。そこで、
プラグインは「何を転送するか」を宣言するだけにして、あなたのマシンで動く小さな
ローカルエージェントが実際の転送とページの表示を行います。つまりこのモードは、
**2 台とも**にインストールと設定が必要です。

リモートホスト側:

1. そのマシンでプラグインを入れます（`herdr plugin install miko-misa/herdr-portfwd`
   と `herdr server reload-config`）。アクションも、プラグインのペインも、
   Ctrl+クリックのリンクハンドラも、すべてそちらで動きます。
2. `~/.config/herdr-portfwd/hosts.toml` を**そちらにも**作ります。どのアクションも、
   自分が動いているマシンの `hosts.toml` からホストを解決するため、これがないと
   リモートのプラグインは *"no hosts are configured"* で失敗し、エージェント宛の
   リクエストは何も積まれません:

   ```toml
   [hosts.buildbox]
   target = "buildbox"
   ```

   `target` はファイル形式上の必須キーですが、リモート側がそれを使って接続すること
   はありません（トンネルを張るのはリモートではないからです）。一致している必要が
   あるのは**名前**です。エージェントは、自分が担当しているホストの名前（または
   target）と一致するリクエストだけを適用します。`attach` に渡す名前を使ってください。

あなたのマシン側:

3. こちらにもインストールします。同じ `herdr plugin install` が `herdr-portfwd`
   CLI を `~/.local/bin` に置き、こちら側に必要なのはそれだけです。先へ進む前に
   `herdr-portfwd --version` が実際に動くか確認してください（動かないときは
   [インストール](#インストール)の項を参照）。`~/.config/herdr-portfwd/hosts.toml`
   には同じ名前のホストを、あなたの `ssh` が実際に到達できる `target` で書きます。
   `attach buildbox` だけなら、その名前をそのまま ssh の宛先として使うフォール
   バックが効きますが、こちら側の `status`・`list`・`stop` はこのファイルから
   ホストを解決するので、書いておいてください。
4. `herdr --remote` を直接叩く代わりに、ラッパー経由でセッションを開始します:

   ```bash
   herdr-portfwd attach buildbox
   ```

   `attach` はそのホスト用の SSH ControlMaster を自前で立ち上げ、ローカル
   エージェントをバックグラウンドで起動し（出力はコントロールソケットの隣の
   `<socket>.agent.log` に出ます）、そのうえで `herdr --remote buildbox` を
   あなたの端末で実行して待ちます。名前に反して `exec` はしません。セッションより
   長く生き残って、エージェントを止め、転送を取り消し、マスターを落とす者が必要だから
   です（正常終了時・シグナル時・パニック時のいずれでも行います）。ホスト名より後ろの
   引数はそのまま渡されます: `herdr-portfwd attach buildbox --session work`。

そして残るのがキーバインドです。ここが一番引っかかる点なので、後述の
[キーバインド](#キーバインド)を読んでください。素の `herdr --remote` では、
プラグインアクションに割り当てたキーはどちら側の設定でも発火しません。

## 使い方

**URL を Ctrl+クリック。** ペイン内の `http://localhost:PORT`、
`http://127.0.0.1:PORT`、`http://[::1]:PORT`（`https` も可）がプラグインに渡り、
転送してからあなたのマシンで開きます。Cmd+クリックではなく Ctrl+クリックなのは、
端末のマウスレポートが Cmd+クリックをただのクリックと区別できないためで、これは
全プラットフォーム共通です。

その先はモードで変わります。herdr-mirror ならプラグインはすでにあなたのマシンに
いるので、転送と URL のブラウザ受け渡しを一度に行います。`herdr --remote` では
クリックはリモート側で処理され、通知を出したうえで URL 付きの転送リクエストを
積みます。実際に転送してブラウザを開くのは、あなたのローカルエージェントの次の
ポーリング——最大で `poll_seconds`、既定 5 秒後です。エージェントが動いていなければ
何も開きません。そのための `herdr-portfwd attach` です。

**一覧から選ぶ。** `scan` アクションが、選択されたホストの帰属済みリスナーを
ピッカーのペインに表示し、選んだものを転送します。

**動いているものを管理する。** `list` アクションが、選択されたホストの生きている
転送すべてと、その持ち主のペイン、停止キーをオーバーレイで表示します。どちらも
一度に 1 ホストが対象です——呼び出しから解決されたホスト、または `--host` が
指定したホストです。

### アクション

| アクション | 動作 |
| --- | --- |
| `portfwd.scan` | 帰属済みのリモートリスナーをピッカーで表示し、選んだものを転送 |
| `portfwd.add` | ポートを転送（ポート未指定ならピッカーを開く） |
| `portfwd.list` | 生きている転送のオーバーレイ（停止キー付き） |
| `portfwd.stop` | 選んだ転送を停止 |
| `portfwd.stop-all` | 現在のホストの転送をすべて停止 |
| `portfwd.open-url` | リンクハンドラの飛び先。転送してからローカルで開く |

### キーバインド

既定のキーは割り当てていません。どこに書くかは、そのセッションで実際に効いている
キーバインドがどれかで決まります。これは「プラグインがどこで動くか」とは別の話です。

**herdr-mirror なら**、ローカルの `~/.config/herdr/config.toml` に書きます:

```toml
# f は forward の f。Herdr の既定はほとんどの文字を使っており、
# prefix+p は previous_tab、prefix+shift+p は rename_pane です。選ぶ前に
# `herdr --default-config | grep prefix+` で確認してください。
[[keys.command]]
key = "prefix+f"
type = "plugin_action"
command = "portfwd.scan"
description = "forward a remote port"

[[keys.command]]
key = "prefix+shift+f"
type = "plugin_action"
command = "portfwd.list"
description = "manage port forwards"
```

そのあと `herdr server reload-config` を実行します。

存在しないアクション ID を割り当てても**報告されません**。設定の再読み込みは黙って
受け入れ、押しても何も起きないだけです。キーが死んでいるように見えたら、ID を
`herdr plugin action list --plugin portfwd` と突き合わせてください。

**素の `herdr --remote` では、カスタムコマンドのキーバインドはどちら側のものも
一切発火しません。** `herdr --remote` の既定は `--remote-keybindings local` で、
クライアントは**あなたの**キーバインドプロファイルをリモートサーバーへ送りますが、
そのプロファイルは `[[keys.command]]` を——`type = "plugin_action"` も含めて——
一切含まない形で作られます（それらのコマンドはリモートで実行されることになるため
Herdr は送らず、サーバー側も受信時にもう一度そのリストを消します）。さらに、
あなたのプロファイルはアタッチしている間サーバー自身のものを**置き換える**ので、
リモートの `[[keys.command]]` も効きません。どちらのファイルに書いても発火しない、
ということです。代わりに使える手が 2 つあります:

- **リモートのキーバインドを要求し**、リモート側に書く:

  ```bash
  herdr-portfwd attach buildbox --remote-keybindings server
  ```

  こうするとリモートサーバーは自分のプロファイルを保つので、上の設定ブロックを
  リモートの `~/.config/herdr/config.toml` に書けば効きます（そちらで
  `herdr server reload-config` を実行してください）。引き換えになるのは
  プロファイル全体です。prefix キーもその他のキーも、そのセッションの間は
  リモートの設定のものになります。

- **あるいはキーバインドを使わない。** Ctrl+クリックにキーバインドは不要です。
  リンクハンドラはプラグインのマニフェスト由来で、プラグインがインストールされて
  いるサーバーが解決するものであり、キーバインドプロファイルとは無関係だからです。
  また、どのアクションもそのセッションのペインから名前で呼び出せます:

  ```bash
  herdr plugin action list --plugin portfwd
  herdr plugin action invoke portfwd.scan
  herdr-portfwd add 5173     # 提示の通知が実行を促してくるのはこれ
  ```

## 設定

`~/.config/herdr-portfwd/config.toml`。コマンドが動いているマシンのものを読みます。
`herdr --remote` では、エージェント向けの設定は**あなたの**マシンのもの、`scan` の
設定はリモートのものになります:

```toml
# ローカルエージェントがリクエストと新しいリスナーを見に行く間隔。0 で検出のみ無効化
# （リクエストは 5 秒ごとに拾います）。エージェントが動く側で読まれます。
poll_seconds = 5

# 検出したポートを Herdr の通知で提示する。既定: true
# エージェントが動く側で読まれ、通知はあなたの Herdr セッションに出ます。
offer_new_listeners = true

# Herdr のペインに帰属するリスナーだけを対象にする。既定: true
# エージェントと、`scan` が動く側の両方で読まれます。
require_pane_attribution = true

# オプトインの自動転送。ワークスペース単位で照合。既定では空で、実行するのは
# エージェントだけです。
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

**1 台につき 1 つのディレクトリ。** 記録は `$XDG_STATE_HOME/herdr-portfwd`、なければ
`~/.local/state/herdr-portfwd`——コントロールソケットと同じディレクトリ——に置かれます。
キーバインドからでも、プラグインのペインからでも、イベントフックからでも、ローカル
エージェントからでも、あなたのシェルからでも同じです。`HERDR_PLUGIN_STATE_DIR` は
意図的に見ません。Herdr がそれを渡すのはプラグインのコマンドに対してだけなので、
従うとキーバインドとシェルで別々の記録ができてしまい、ポートが開いているのに
`herdr-portfwd status` が「転送なし」と報告する状態になっていました。`status` が
表示するパスが、そのマシン上のすべてが使うパスです。

**ただし 2 台なら、ディレクトリも 2 つあります。** `herdr --remote` がまさにそれです。
`forwards.json` を書くのはポートを開いた側、つまりあなたのマシンのローカル
エージェント。`requests.json` を書くのはプラグイン、つまりリモート側です。だから
訊く相手を選んでください:

- あなたのマシンで実行する `herdr-portfwd status` が、実際にどのポートが転送されて
  いてマスターが生きているかの答えです。
- リモート側（そのセッションのペイン、または `list` アクション）の `status` は、
  プラグインが積んで、まだ決着がついていないものを示します。向こう側でポートを開く
  ものは何もないので、記録された転送は空です。

「待ち」に数えるのは、まだ誰も処理していないものだけです。ローカルエージェントは
どこまで処理したかを `requests.json` の隣の `requests.acked` に記録し、キューを
読むたびにその印を折り込むので、すでに適用済みのリクエストは数から外れます——
`herdr --remote` で `status` が動くリモートホスト側でも同じです。キューがいつまでも
減らないなら、エージェントが動いていないか、読んでいるリクエストファイルの隣に
書き込めていません。

ログは 2 つ知っておく価値があります。ローカルエージェント自身の出力は、ソケットの
隣の `<コントロールソケット>.agent.log` に出ます——Ctrl+クリックで転送は積まれたのに
ブラウザが開かないときに見る場所です。Herdr が実行したコマンドの記録は
`herdr plugin log list --plugin portfwd` にあり、こちらはサーバーが動いているマシン
側——`herdr --remote` ならリモート側——です。

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

こちらで何もクリックしなくてもリモート側が手元のマシンに届かせられる唯一の経路が、
ブラウザへの受け渡しです。`herdr --remote` では積まれたリクエストが URL を運べて、
それを開くのはローカルエージェントだからです。その URL はこちら側で loopback として
再検証され、そのパスで実際に bind したローカルポートを指していることを要求され、
`open` / `xdg-open` には argv 1 語として渡されます。それで何が防げて何が防げないか、
そして脆弱性の報告手順は [SECURITY.md](SECURITY.md) にあります。

## 開発

```bash
just check      # fmt + clippy + テスト
just test
cargo build --release
herdr plugin link .          # この作業ツリーをリンク
herdr server reload-config
```

テストは実際の `ssh`・実際の `herdr`・実際のブラウザ・ネットワークを一切叩きません。
この 3 つのプログラムはいずれもトレイト——`SshRunner`・`HerdrClient`・
`UrlOpener`——越しに扱われ、テストではフェイクに差し替わります。設計とその不変条件は
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)、プルリクエスト前に
[CONTRIBUTING.md](CONTRIBUTING.md) を参照してください。

## ライセンス

MIT — [LICENSE](LICENSE) を参照。
