# YuiLang

AI駆動開発で効率よく書けることを目指すプログラミング言語です。

- AIが間違えにくい: 見慣れた構文、書き方は一通り、暗黙の挙動なし
- 間違いをすぐ検出できる: 静的型、網羅必須の `match`、言語組み込みのテストと契約
- 機械が読める: すべてのエラーにコード・位置・修正ヒントがあり、JSONで出力できる

```
fn divide(a: Int, b: Int) -> Result<Int, String> {
  if b == 0 {
    return Err("division by zero")
  }
  Ok(a / b)
}

test "divide" {
  assert divide(6, 2) == Ok(3)
  assert divide(1, 0) == Err("division by zero")
}

fn main() uses io {
  match divide(10, 2) {
    Ok(v) => print("result: {v}")
    Err(e) => print("error: {e}")
  }
}
```

言語仕様は [SPEC.md](SPEC.md)、サンプルは [examples](examples) にあります。

## 現在の状態

v0.1。型チェッカ付きのツリーウォーク型インタプリタです。処理系は Rust 製で、外部クレートへの依存はありません。
1ファイルのプログラムだけを扱えます（モジュール、ジェネリクス、クロージャは未実装）。

## ビルド

[Rust](https://www.rust-lang.org/) が必要です。

```
cargo build --release
```

`target/release/yui`（Windows では `yui.exe`）ができます。

## 使い方

```
yui check   <file> [--json]   型チェックのみ
yui run     <file> [--json]   main を実行
yui test    <file> [--json]   test ブロックを実行
yui outline <file>            宣言のシグネチャだけを表示
```

```
$ yui run examples/hello.yui
result: 5
error: division by zero

$ yui test examples/hello.yui
ok    divide
1 passed, 0 failed
```

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

## 処理系のテスト

```
cargo test
```

`examples/` のプログラムがすべて通ることと、`tests/errors/` の各ファイルが
1行目に書かれたエラーコードで失敗することを確認します。

## ライセンス

[MIT License](LICENSE)
