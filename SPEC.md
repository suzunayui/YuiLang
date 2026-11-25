# YuiLang 言語仕様 v0.1

YuiLang は、AIが書き、AIが直すことを前提に設計した静的型付き言語です。
この文書は v0.1 の処理系（`yui`）が実装している範囲を記述します。

## 1. 設計原則

1. **見慣れた構文** — Rust / TypeScript / Swift 系の構文に寄せ、独自記法を増やさない。
2. **書き方は一通り** — 同じことを書く構文を複数用意しない。セミコロンなし、文は1行に1つ。
3. **暗黙の挙動なし** — 暗黙の型変換、null、例外、グローバル変数、オーバーロードはない。
4. **値セマンティクス** — すべての値はコピーとして振る舞い、参照の共有（エイリアス）は起きない。
5. **副作用は型に出る** — 入出力を行う関数は `uses io` を宣言する。
6. **検証を言語に組み込む** — `test`、`assert`、`requires`、`ensures` が構文にある。
7. **機械可読な診断** — すべてのエラーは安定したコード・位置・修正のヒントを持ち、JSONで出力できる。

## 2. 字句

- ソースは UTF-8。識別子は ASCII の `[A-Za-z_][A-Za-z0-9_]*`。
- **型名は大文字始まり、変数名・関数名は小文字始まり**（強制）。
- コメントは `//` から行末まで。
- セミコロンは使わない。文と文は改行で区切る。
- 式が次の行に続くのは、行末が演算子・開き括弧で終わる場合と、次の行が `.` で始まる場合だけ。

```
let total = price *
  count          // OK: 行末が演算子

let n = items
  .len()         // OK: 次の行が . で始まる
```

### リテラル

| 種類 | 例 |
| --- | --- |
| Int（64bit符号付き） | `42`, `1_000_000` |
| Float（64bit） | `3.14`, `2.0` |
| Bool | `true`, `false` |
| String | `"hello"`, `"x = {x}"` |
| List | `[1, 2, 3]`, `[]` |

文字列の中の `{式}` は式の値に置き換わる（文字列補間）。文字列の連結は補間で行う。
エスケープは `\n` `\t` `\r` `\\` `\"` `\{` `\}`。

## 3. 型

| 型 | 説明 |
| --- | --- |
| `Int`, `Float`, `Bool`, `String` | 基本型 |
| `Unit` | 値がないことを表す型 |
| `List<T>` | リスト |
| `Option<T>` | `Some(x)` または `None` |
| `Result<T, E>` | `Ok(x)` または `Err(e)` |
| `struct` で定義した型 | 名前付きフィールドの組 |
| `enum` で定義した型 | 複数のバリアントのどれか |

- 関数の引数と戻り値の型注釈は必須。`let` の型注釈は省略できる（推論される）。
- `Int` と `Float` は自動変換されない。`.to_float()` / `.to_int()` を使う。
- v0.1 にはユーザー定義のジェネリクス、クロージャ、メソッド定義、モジュールはない。

## 4. 宣言

```
struct Point {
  x: Int,
  y: Int,
}

enum Shape {
  Circle(Float),
  Rect(Float, Float),
  Empty,
}

fn area(s: Shape) -> Float {
  match s {
    Shape.Circle(r) => 3.14159 * r * r
    Shape.Rect(w, h) => w * h
    Shape.Empty => 0.0
  }
}
```

- 構造体の生成は `Point { x: 1, y: 2 }`。すべてのフィールドを必ず指定する。
- enum の値は常に `型名.バリアント名` で書く（`Shape.Circle(1.0)`）。
- `Some` `None` `Ok` `Err` だけは修飾なしで書く。
- トップレベルに置けるのは `fn` `struct` `enum` `test` だけ。宣言の順序は自由。

### 関数

```
fn divide(a: Int, b: Int) -> Result<Int, String> {
  if b == 0 {
    return Err("division by zero")
  }
  Ok(a / b)
}
```

- ブロックの最後の式がそのブロックの値になる。
- 戻り値の型を省略すると `Unit`。
- 関数は第一級の値ではない（v0.1）。

### 副作用

```
fn greet(name: String) uses io {
  print("hello, {name}")
}
```

`uses io` を宣言していない関数から、`uses io` の関数を呼ぶとコンパイルエラーになる。
シグネチャを見るだけで、その関数が入出力をするかどうかがわかる。

### 契約

```
fn clamp(x: Int, lo: Int, hi: Int) -> Int
  requires lo <= hi
  ensures result >= lo && result <= hi
{
  if x < lo { lo } else if x > hi { hi } else { x }
}
```

- `requires` は呼び出し時に、`ensures` は戻るときに実行時検査される。
- `ensures` の中では戻り値を `result` で参照できる。
- 契約の式は副作用を持てない。

## 5. 文

| 文 | 例 |
| --- | --- |
| 変数宣言（不変） | `let x = 1` |
| 変数宣言（可変） | `let mut x = 1` |
| 代入 | `x = 2`, `p.x = 3`, `xs[0] = 4` |
| 繰り返し | `while cond { ... }` |
| 範囲の繰り返し | `for i in 0..10 { ... }`（10は含まない） |
| リストの繰り返し | `for item in items { ... }` |
| 表明 | `assert x > 0` |
| 式文 | `print("hi")` |

- 代入できるのは `let mut` で宣言した変数（とそのフィールド・要素）だけ。
- 同じスコープで同じ名前を再宣言することはできない。
- **`Unit` 以外の値を捨てる式文はエラー**。意図して捨てる場合は `let _ = 式` と書く。
  値セマンティクスでは `xs.push(1)` は新しいリストを返すだけなので、
  代入し忘れをコンパイル時に検出するための規則。

## 6. 式

### 演算子（優先順位の低い順）

| 演算子 | 対象 |
| --- | --- |
| `\|\|` | Bool |
| `&&` | Bool |
| `==` `!=` `<` `<=` `>` `>=` | 同じ型どうし（連鎖不可） |
| `+` `-` | Int または Float |
| `*` `/` `%` | Int または Float |
| 単項 `-` `!` | 数値 / Bool |
| `.f` `.m()` `[i]` `?` | 後置 |

- `Int` の演算はオーバーフローすると実行時エラー。ゼロ除算も実行時エラー。
- `xs[i]` は範囲外で実行時エラー。安全に取り出すには `xs.get(i)`（`Option<T>` を返す）。

### if / match

`if` と `match` は式。値として使う `if` には `else` が必要。

```
let label = if n == 0 { "zero" } else { "many" }

let text = match find(id) {
  Some(user) => user.name
  None => "unknown"
}
```

パターン: `_`、変数束縛、Int / String / Bool リテラル、`Some(p)` `None` `Ok(p)` `Err(p)`、
`Enum.Variant(p, ...)`。`match` は**網羅していなければコンパイルエラー**。

### `?` 演算子

`Result` / `Option` を返す関数の中で、`式?` は `Err` / `None` ならそのまま return し、
そうでなければ中身を取り出す。

```
fn parse_pair(a: String, b: String) -> Option<Int> {
  let x = a.to_int()?
  let y = b.to_int()?
  Some(x + y)
}
```

### return / break / continue

`return 式`、`break`、`continue` は式として書ける（`match` の腕などに置ける）。

## 7. 組み込み

### 関数

| 関数 | 型 |
| --- | --- |
| `print(s)` | `(String) -> Unit uses io` |
| `read_line()` | `() -> String uses io` |
| `panic(msg)` | `(String) -> Never` |

### メソッド

| 対象 | メソッド |
| --- | --- |
| すべての型 | `to_string() -> String` |
| `String` | `len() -> Int`, `contains(String) -> Bool`, `trim() -> String`, `to_int() -> Option<Int>`, `to_float() -> Option<Float>` |
| `List<T>` | `len() -> Int`, `push(T) -> List<T>`, `get(Int) -> Option<T>`, `contains(T) -> Bool` |
| `Int` | `to_float() -> Float` |
| `Float` | `to_int() -> Int`（切り捨て） |
| `Option<T>` | `unwrap_or(T) -> T`, `is_some() -> Bool`, `is_none() -> Bool` |
| `Result<T, E>` | `unwrap_or(T) -> T`, `is_ok() -> Bool`, `is_err() -> Bool` |

## 8. テスト

```
test "divide" {
  assert divide(6, 2) == Ok(3)
  assert divide(1, 0) == Err("division by zero")
}
```

`test` ブロックは `yui test` で実行される。比較の `assert` が失敗すると、左右の値が表示される。
入出力を行うテストは `test "name" uses io { ... }` と書く。

## 9. ツール

```
yui check   <file> [--json]   型チェックのみ
yui run     <file> [--json]   main を実行
yui test    <file> [--json]   test ブロックを実行
yui outline <file>            宣言のシグネチャだけを表示
```

- `yui run` は `fn main()`（引数なし、戻り値 `Unit`）を実行する。
- `--json` を付けると、診断やテスト結果を1個のJSONとして標準出力に出す。
  `yui run` だけは標準出力をプログラム自身の出力に使うので、診断のJSONは標準エラーに出す。
- 失敗時の終了コードは 1。

### 診断の形式

```json
{
  "ok": false,
  "diagnostics": [
    {
      "code": "E0305",
      "message": "unused value of type `List<Int>`",
      "file": "main.yui",
      "line": 3, "column": 3, "end_line": 3, "end_column": 13,
      "hint": "assign the result, or discard it explicitly with `let _ = ...`"
    }
  ]
}
```

### エラーコード

| 範囲 | 種類 |
| --- | --- |
| `E00xx` | 字句エラー |
| `E01xx` | 構文エラー |
| `E02xx` | 名前解決（未定義、重複） |
| `E03xx` | 型エラー |
| `E04xx` | 副作用エラー |
| `E05xx` | 実行時エラー（assert、契約、オーバーフローなど） |

## 10. 今後の予定

- 複数ファイル（モジュール）
- ユーザー定義ジェネリクスとクロージャ
- `yui fmt`
- バイトコードVM / WASM出力
