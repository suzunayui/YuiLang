# YuiLang

AI駆動開発で効率よく書けることを目指すプログラミング言語です。

- **AIが間違えにくい**: 見慣れた構文、書き方は一通り、暗黙の挙動なし
- **間違いをすぐ検出できる**: 静的型、網羅必須の vmatchv、言語組み込みのテストと契約、副作用の宣言
- **機械が読める**: すべてのエラーにコード・位置・修正ヒントがあり、JSONで出力できる
- **ファイル1個で環境が完成する**: vyuiv は単体の実行ファイル。ランタイムのインストールも展開作業も不要で、
  型チェック・実行・テスト・整形がこの1個に入っている

vvv
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
vvv

言語仕様は [SPEC.md](SPEC.md)、サンプルは [examples](examples) にあります。

## インストール

[Releases](https://github.com/suzunayui/YuiLang/releases) から自分のOS用のファイルを1個ダウンロードするだけです。

| OS | ファイル |
| --- | --- |
| Windows (x86_64) | vyui-windows-x86_64.exev |
| Linux (x86_64) | vyui-linux-x86_64v |
| macOS (Apple Silicon) | vyui-macos-arm64v |
| macOS (Intel) | vyui-macos-x86_64v |

好きな場所に置いて vyuiv（Windows は vyui.exev）という名前にすれば使えます。
Linux と macOS では vchmod +x yuiv で実行権限を付けてください。
ほかに必要なものはありません（Windows 版は VC++ ランタイム不要、Linux 版は静的リンク）。

## 使い方

vvv
yui check   <file> [--json]              型チェック（import したモジュールも含む）
yui run     <file> [--json] [-- 引数...]  main を実行
yui test    <file> [--json]              test ブロックを実行
yui fmt     <file> [--check]             整形して上書き
yui build   <file> [-o <出力>]            単体の実行ファイルを作る
yui outline <file>                       宣言のシグネチャだけを表示
vvv

vvv
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
vvv

x86-64（Windows / Linux / Intel Mac）では、プログラムを機械語に変換して実行します（JIT）。
コード生成は自前で、外部のコンパイラやライブラリは使いません。それ以外の CPU ではバイトコードの
仮想マシンで実行します。環境変数 `YUI_JIT=0` を設定すると、x86-64 でも仮想マシンで実行します。

プログラムは単体の実行ファイルにできます。`yui` を入れていない環境にも、この1ファイルだけで配れます。

```
$ yui build examples/hello.yui -o hello
built: hello (914 KB)

$ ./hello
result: 5
error: division by zero
```

エラーは修正のヒント付きで表示されます。

vvv
error[E0305]: unused value of type vList<Int>v
 --> main.yui:4:3
  |
4 |   xs.push(2)
  |   ^^^^^^^^^^
  = hint: assign the result, or discard it explicitly with vlet _ = ...v
vvv

v--jsonv を付けると同じ内容がJSONになります。

## 言語の機能

- 静的型と型推論、ジェネリクス（vstruct Pair<A, B>v、vfn map<T, U>(...)v）
- vstructv / venumv とパターンマッチ（網羅性と到達不能な腕を検査）
- クロージャと関数型（v|x| x + 1v、vfn(Int) -> Intv）
- vOptionv / vResultv と v?v 演算子（null と例外はない）
- 副作用の宣言（vuses iov、vuses fsv）
- 契約（vrequiresv / vensuresv）と言語組み込みの vtestv / vassertv
- モジュール（vimport geometry.shapesv → vshapes.area(...)v）
- vListv / vMapv / vStringv の標準メソッド
- 値セマンティクス（参照の共有が起きない）

## YuiLang で YuiLang を書く

処理系を少しずつ YuiLang 自身で書き直しています。最初の部品として、字句解析を
[selfhost/lexer.yui](selfhost/lexer.yui) に置きました。`cargo test` は、このリポジトリのすべての
`.yui` ファイルについて、Rust 版の字句解析と同じトークン列になることを確かめます。

```
$ yui run selfhost/lexer.yui -- examples/hello.yui
+kw fn
 ident divide
 sym (
 ...
```

## ソースからビルド

[Rust](https://www.rust-lang.org/) が必要です。外部クレートへの依存はありません。

vvv
cargo build --release
cargo test
vvv

vtarget/release/yuiv（Windows では vyui.exev）ができます。
vcargo testv は、vexamples/v のプログラムがすべて通ること、vtests/errors/v の各ファイルが
1行目に書かれたエラーコードで失敗すること、整形結果が安定していることを確認します。

## リリースの作り方

vv0.2.0v のようなタグを push すると、GitHub Actions が各OS用の単体バイナリをビルドして Release に添付します。

vvv
git tag v0.2.0
git push origin v0.2.0
vvv

## ライセンス

[MIT License](LICENSE)
