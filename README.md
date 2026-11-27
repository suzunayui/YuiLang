# YuiLang

AI駆動開発で効率よく書けることを目指すプログラミング言語です。

- **AIが間違えにくい**: 見慣れた構文、書き方は一通り、暗黙の挙動なし
- **間違いをすぐ検出できる**: 静的型、網羅必須の `match`、言語組み込みのテストと契約、副作用の宣言
- **機械が読める**: すべてのエラーにコード・位置・修正ヒントがあり、JSONで出力できる
- **ファイル1個で環境が完成する**: `yui` は単体の実行ファイル。ランタイムのインストールも展開作業も不要で、
  型チェック・実行・テスト・整形がこの1個に入っている

```
fn divide(a: Int, b: Int) -> Result<Int, String> {
  if b == 0 { return Err("division by zero") }
  Ok(a / b)
}

test "divide" {
  assert divide(6, 2) == Ok(3)
  assert divide(1, 0) == Err("division by zero")
}

fn main() uses io {
  let results = [10, 0, 4].map(|b| divide(100, b))
  for r in results {
    match r {
      Ok(v) => print("result: {v}")
      Err(e) => print("error: {e}")
    }
  }
}
```

言語仕様は [SPEC.md](SPEC.md)、サンプルは [examples](examples) にあります。

## インストール

[Releases](https://github.com/suzunayui/YuiLang/releases) から自分のOS用のファイルを1個ダウンロードするだけです。

| OS | ファイル |
| --- | --- |
| Windows (x64) | `yui-windows-x64.exe` |
| Linux (x64) | `yui-linux-x64` |
| macOS (Apple Silicon) | `yui-macos-arm64` |
| macOS (Intel) | `yui-macos-x64` |

好きな場所に置いて `yui`（Windows は `yui.exe`）という名前にすれば使えます。
Linux と macOS では `chmod +x yui` で実行権限を付けてください。
ほかに必要なものはありません（Windows 版は VC++ ランタイム不要、Linux 版は静的リンク）。

## 使い方

```
yui check   <file> [--json]              型チェック（import したモジュールも含む）
yui run     <file> [--json] [-- 引数...]  main を実行
yui test    <file> [--json]              test ブロックを実行
yui fmt     <file> [--check]             整形して上書き
yui build   <file> [-o <出力>]            単体の実行ファイルを作る（--native / --bundle で方式を指定）
yui outline <file>                       宣言のシグネチャだけを表示
```

```
$ yui run examples/hello.yui
result: 5
error: division by zero

$ yui test examples/generics.yui
ok    generic struct
ok    generic enum
ok    closures
ok    captures are copies
ok    list pipelines
5 passed, 0 failed
```

x64（Windows / Linux / Intel Mac）では、プログラムを機械語に変換して実行します（JIT）。
コード生成は自前で、外部のコンパイラやライブラリは使いません。それ以外の CPU ではバイトコードの
仮想マシンで実行します。環境変数 `YUI_JIT=0` を設定すると、x64 でも仮想マシンで実行します。

プログラムは単体の実行ファイルにできます。`yui` を入れていない環境にも、この1ファイルだけで配れます。

```
$ yui build examples/hello.yui -o hello.exe
built: hello.exe (3584 bytes, native)

$ ./hello.exe
result: 5
error: division by zero
```

Windows x64 では、プログラムを機械語に変換して、インタプリタを含まない実行ファイルを直接書き出します
（数 KB〜十数 KB）。コード生成も実行ファイルの書き出しも自前で、外部のコンパイラやリンカは使いません。

次の場合は、`yui` 自身にプログラムを同梱した実行ファイルになります（約 1.3 MB）。動作は同じです。

- Windows x64 以外（Linux、macOS）
- 機械語への変換がまだ対応していない機能を使うプログラム（文字列から小数への変換など、ごく一部）。
  このときは、理由を1行表示してから同梱方式でビルドします

`--native` を付けると、同梱方式に切り替える代わりにエラーにします。`--bundle` を付けると、常に同梱方式にします。
設計と実測は [docs/native-backend.md](docs/native-backend.md) にあります。

### ゲームを作る

ウィンドウを開いて点を描き、キーボードとマウスを読む関数が組み込みで入っています（いまは Windows だけ）。
三角形を GPU に渡して 3 次元の絵を描く関数もあります。
[games](games) に見本があります。

```
$ yui run games/blocks.yui                  その場で動かす
$ yui build games/blocks.yui -o blocks.exe  45 KB の実行ファイルにする
```

- [games/square.yui](games/square.yui): 四角を矢印キーで動かす、いちばん小さな例
- [games/blocks.yui](games/blocks.yui): 横から見たブロックの世界。歩く・跳ぶ・掘る・置く
- [games/blocks3d.yui](games/blocks3d.yui): 3 次元のブロックの世界。歩き回って、壊したり置いたりできる。
  計算が多いので `yui build` で実行ファイルにしてから遊ぶ（63 KB、1 秒に約 76 コマ）
- [games/blocks_gpu.yui](games/blocks_gpu.yui): 同じ世界を GPU で描く版。世界は 16 倍の広さ（128 × 128 マス）、
  絵は 1280 × 720（引数で 1920 × 1080 にもできる）で、1 秒に 60 コマ。実行ファイルは 86 KB
- [games/blocks_ai.yui](games/blocks_ai.yui): AI の仲間「ユイ」と一緒に遊ぶ版。Enter でお願いを書くと
  （「木を集めてきてー」「かわいいおうち建ててー」「お城建ててー」）、ユイが木を集めたり家や城を建てたりする。
  建物は、自由な形の部品（箱・円柱・円すい・三角屋根・丸屋根・球・斜面）を好きな大きさ・色・向きで組んだ
  3D モデルとして、少しずつ組み上がる。中に入れて、`/undo` で取り消せる。
  ユイの頭脳は Claude で、環境変数 `ANTHROPIC_API_KEY` に API キーが要る（なければ決まった返事だけ）。
  通信も JSON も文字の表示も YuiLang だけで書いてあり、ほかの言語や道具は使わない

使える関数は [SPEC.md](SPEC.md) の「ウィンドウ・入力・時刻・乱数」と「GPU で 3 次元の絵を描く」にあります。

エラーは修正のヒント付きで表示されます。

```
error[E0305]: unused value of type `List<Int>`
 --> main.yui:4:3
  |
4 |   xs.push(2)
  |   ^^^^^^^^^^
  = hint: assign the result, or discard it explicitly with `let _ = ...`
```

`--json` を付けると同じ内容がJSONになります。

## 言語の機能

- 静的型と型推論、ジェネリクス（`struct Pair<A, B>`、`fn map<T, U>(...)`）
- `struct` / `enum` とパターンマッチ（網羅性と到達不能な腕を検査）
- クロージャと関数型（`|x| x + 1`、`fn(Int) -> Int`）
- `Option` / `Result` と `?` 演算子（null と例外はない）
- 副作用の宣言（`uses io`、`uses fs`、`uses net`）
- 契約（`requires` / `ensures`）と言語組み込みの `test` / `assert`
- モジュール（`import geometry.shapes` → `shapes.area(...)`）
- `List` / `Map` / `String` の標準メソッド
- 値セマンティクス（参照の共有が起きない）

## YuiLang で YuiLang を書く

処理系を少しずつ YuiLang 自身で書き直しています。いまあるのは字句解析
（[selfhost/lexer.yui](selfhost/lexer.yui)）と構文解析（[selfhost/parser.yui](selfhost/parser.yui)）です。
`cargo test` は、このリポジトリのすべての `.yui` ファイルについて、Rust 版と同じトークン列・
同じ構文木（構文エラーなら同じエラーコード）になることを確かめます。

```
$ yui run selfhost/lexer.yui -- examples/hello.yui
+kw fn
 ident divide
 sym (
 ...
```

## ソースからビルド

[Rust](https://www.rust-lang.org/) が必要です。外部クレートへの依存はありません。

```
cargo build --release
cargo test
```

`target/release/yui`（Windows では `yui.exe`）ができます。
`cargo test` は、`examples/` のプログラムがすべて通ること、`tests/errors/` の各ファイルが
1行目に書かれたエラーコードで失敗すること、整形結果が安定していることを確認します。

## リリースの作り方

バージョンは `年4桁.月日4桁.連番` です（例: 2026年11月25日の1回目のリリースは `2026.1125.1`）。

1. [Cargo.toml](Cargo.toml) の `version` を書き換える。Cargo は先頭のゼロを許さないので、
   1月5日なら `2026.105.1` と書く（`yui --version` とタグでは `2026.0105.1` と表示される）。
2. `v` を付けたタグを push する。

```
git tag v2026.1125.1
git push origin v2026.1125.1
```

GitHub Actions が各OS用の単体バイナリをビルドして Release に添付します。
タグと `yui --version` が一致しない場合は、リリースを作らずに失敗します。

## ライセンス

[MIT License](LICENSE)
