# YuiLang 言語仕様

YuiLang は、AIが書き、AIが直すことを前提に設計した静的型付き言語です。
この文書は、同じリポジトリにある処理系（`yui`）が実装している範囲を記述します。
バージョンは `yui --version` で確認できます。

## 1. 設計原則

1. **見慣れた構文** — Rust / TypeScript / Swift 系の構文に寄せ、独自記法を増やさない。
2. **書き方は一通り** — 同じことを書く構文を複数用意しない。セミコロンなし、文は1行に1つ。整形は `yui fmt` が決める。
3. **暗黙の挙動なし** — 暗黙の型変換、null、例外、グローバル変数、オーバーロードはない。
4. **値セマンティクス** — すべての値はコピーとして振る舞い、参照の共有（エイリアス）は起きない。
5. **副作用は型に出る** — 入出力を行う関数は `uses io` などを宣言する。
6. **名前の出どころが見える** — 他のファイルの宣言は必ず `モジュール名.名前` と書く。
7. **検証を言語に組み込む** — `test`、`assert`、`requires`、`ensures` が構文にある。
8. **機械可読な診断** — すべてのエラーは安定したコード・位置・修正のヒントを持ち、JSONで出力できる。
9. **ファイル1個で動く** — 処理系は単体の実行ファイルで、ランタイムの別途インストールは不要。

## 2. 字句

- ソースは UTF-8。識別子は ASCII の `[A-Za-z_][A-Za-z0-9_]*`。
- **型名は大文字始まり、変数名・関数名は小文字始まり**（強制）。
- コメントは `//` から行末まで。
- セミコロンは使わない。文と文は改行で区切る。
- 式が次の行に続くのは、行末が演算子・開き括弧・カンマで終わる場合と、次の行が `.` で始まる場合だけ。

### リテラル

| 種類 | 例 |
| --- | --- |
| Int（64bit符号付き） | `42`, `1_000_000` |
| Float（64bit） | `3.14`, `2.0` |
| Bool | `true`, `false` |
| String | `"hello"`, `"x = {x}"` |
| Unit | `()` |
| List | `[1, 2, 3]`, `[]` |
| Map | `Map { "a": 1, "b": 2 }`, `Map {}` |

文字列の中の `{式}` は式の値に置き換わる（文字列補間）。文字列の連結は補間で行う。
エスケープは `\n` `\t` `\r` `\\` `\"` `\{` `\}`。補間の中に文字列リテラルは書けない。

## 3. 型

| 型 | 説明 |
| --- | --- |
| `Int`, `Float`, `Bool`, `String` | 基本型 |
| `Unit` | 値がないことを表す型 |
| `List<T>` | リスト |
| `Map<K, V>` | 連想配列。キーは `Int` `String` `Bool` のいずれか。常にキーの昇順で並ぶ |
| `Option<T>` | `Some(x)` または `None` |
| `Result<T, E>` | `Ok(x)` または `Err(e)` |
| `fn(A, B) -> R` | 関数型。副作用があれば `fn(A) -> R uses io` |
| `struct` で定義した型 | 名前付きフィールドの組 |
| `enum` で定義した型 | 複数のバリアントのどれか |

- 関数の引数と戻り値の型注釈は必須。`let` とクロージャ引数の型注釈は省略できる（推論される）。
- `Int` と `Float` は自動変換されない。`.to_float()` / `.to_int()` を使う。

## 4. 宣言

トップレベルに置けるのは `import` `fn` `struct` `enum` `test` だけ。`import` は先頭に書く。宣言の順序は自由。

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
- ブロックの最後の式がそのブロックの値になる。戻り値の型を省略すると `Unit`。

### ジェネリクス

```
struct Pair<A, B> {
  first: A,
  second: B,
}

enum Tree<T> {
  Leaf,
  Node(Tree<T>, T, Tree<T>),
}

fn swap<A, B>(p: Pair<A, B>) -> Pair<B, A> {
  Pair { first: p.second, second: p.first }
}
```

- 型引数は呼び出し側で推論される。明示的に渡す構文はない。
- 型パラメータの値にできるのは、受け渡し、`==` / `!=`、`to_string()` だけ（トレイト境界はない）。

### 関数型とクロージャ

```
fn apply_twice<T>(f: fn(T) -> T, x: T) -> T {
  f(f(x))
}

fn make_adder(n: Int) -> (fn(Int) -> Int) {
  |x| x + n
}

let add5 = make_adder(5)
let ys = xs.map(|x| x * 2).filter(|x| x > 10)
```

- クロージャは `|引数| 式`。引数なしは `|| 式`、型注釈は `|x: Int|`、複数の文は `|x| { ... }`。
- トップレベルの関数は名前だけで値として渡せる（`apply_twice(double, 3)`）。
- 関数型を戻り値にするときは括弧で囲む: `-> (fn(Int) -> Int)`。
- **クロージャは変数を値でキャプチャする**。作った時点のコピーを持ち、外の変数には代入できない。
- 関数の値を呼べるのは、変数に入っているときだけ（`f(x)`）。

### 副作用

```
fn greet(name: String) uses io {
  print("hello, {name}")
}

fn save(path: String, text: String) -> Result<Unit, String> uses fs {
  write_file(path, text)
}
```

| 効果 | 意味 |
| --- | --- |
| `io` | 標準入出力、コマンドライン引数 |
| `fs` | ファイルの読み書き |

- 宣言していない効果を持つ関数を呼ぶとコンパイルエラーになる。複数あるときは `uses io, fs`。
- クロージャの効果は本体から推論され、関数型に現れる（`|x| print(x)` は `fn(String) uses io`）。
- 効果のない関数は、効果を許す関数型の引数に渡せる。逆はできない。
- 組み込みの `map` `filter` `fold` `any` `all` は、渡した関数の効果をそのまま呼び出し側に要求する。

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

## 5. モジュール

1つのファイルが1つのモジュール。`import` で他のファイルを読み込む。

```
import geometry.shapes     // <root>/geometry/shapes.yui
import textutil            // <root>/textutil.yui

fn describe(s: shapes.Shape) -> String {
  "{textutil.pad_left(shapes.name(s), 8)} area = {shapes.area(s)}"
}

let r = shapes.Shape.Rect(2.0, 3.0)
let p = shapes.Point { x: 1.0, y: 2.0 }
```

- パスはルートフォルダからの相対で、区切りは `.`。ルートは実行するファイルのあるフォルダ（`--root` で変更できる）。
- 読み込んだモジュールは、パスの最後の名前で参照する（`geometry.shapes` → `shapes`）。
- 他のモジュールの関数・型は必ず `モジュール名.名前` と書く。自分のモジュールの宣言は修飾なしで書く。
- モジュールのすべての宣言は外から参照できる（公開・非公開の区別はない）。
- 循環 import は可能。実行するファイル自身を import することはできない。
- `yui test` が実行するのは、指定したファイルの `test` だけ。import したモジュールの `test` は型チェックだけされる。

## 6. 文

| 文 | 例 |
| --- | --- |
| 変数宣言（不変） | `let x = 1` |
| 変数宣言（可変） | `let mut x = 1` |
| 代入 | `x = 2`, `p.x = 3`, `xs[0] = 4`, `m["key"] = 5` |
| 繰り返し | `while cond { ... }` |
| 範囲の繰り返し | `for i in 0..10 { ... }`（10は含まない） |
| リストの繰り返し | `for item in items { ... }` |
| 表明 | `assert x > 0` |
| 式文 | `print("hi")` |

- 代入できるのは `let mut` で宣言した変数（とそのフィールド・要素）だけ。
- 同じスコープで同じ名前を再宣言することはできない。
- `m[key] = v` はキーがなければ追加する。
- **`Unit` 以外の値を捨てる式文はエラー**。意図して捨てる場合は `let _ = 式` と書く。
  値セマンティクスでは `xs.push(1)` は新しいリストを返すだけなので、
  代入し忘れをコンパイル時に検出するための規則。

## 7. 式

### 演算子（優先順位の低い順）

| 演算子 | 対象 |
| --- | --- |
| `\|\|` | Bool |
| `&&` | Bool |
| `==` `!=` `<` `<=` `>` `>=` | 同じ型どうし（連鎖不可）。順序比較は Int / Float / String |
| `+` `-` | Int または Float |
| `*` `/` `%` | Int または Float |
| 単項 `-` `!` | 数値 / Bool |
| `.f` `.m()` `[i]` `?` | 後置 |

- `Int` の演算はオーバーフローすると実行時エラー。ゼロ除算も実行時エラー。
- `xs[i]` は範囲外で、`m[key]` はキーがなければ実行時エラー。安全に取り出すには `.get()`（`Option` を返す）。
- 関数どうしは比較できない。

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
`Enum.Variant(p, ...)`。入れ子にできる。

- `match` は**網羅していなければコンパイルエラー**。足りない値の例がエラーに表示される。
- 前の腕で覆われていて**到達できない腕もコンパイルエラー**。

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
クロージャの中の `return` はクロージャから戻る。

## 8. 組み込み

### 関数

| 関数 | 型 |
| --- | --- |
| `print(s)` | `(String) -> Unit uses io` |
| `read_line()` | `() -> String uses io` |
| `args()` | `() -> List<String> uses io` |
| `read_file(path)` | `(String) -> Result<String, String> uses fs` |
| `write_file(path, text)` | `(String, String) -> Result<Unit, String> uses fs` |
| `panic(msg)` | `(String) -> Never` |

### メソッド

すべての型: `to_string() -> String`

**String**

| メソッド | 戻り値 |
| --- | --- |
| `len()`（文字数）, `is_empty()` | `Int`, `Bool` |
| `contains(s)`, `starts_with(s)`, `ends_with(s)` | `Bool` |
| `trim()`, `to_upper()`, `to_lower()`, `replace(from, to)` | `String` |
| `slice(start, end)`（文字単位、end は含まない） | `String` |
| `split(sep)`, `chars()` | `List<String>` |
| `to_int()`, `to_float()` | `Option<Int>`, `Option<Float>` |

**List\<T\>**

| メソッド | 戻り値 |
| --- | --- |
| `len()`, `is_empty()`, `contains(x)` | `Int`, `Bool`, `Bool` |
| `push(x)`, `concat(other)`, `reverse()`, `slice(start, end)` | `List<T>`（新しいリスト） |
| `sort()`（Int / Float / String のみ） | `List<T>` |
| `get(i)`, `first()`, `last()` | `Option<T>` |
| `join(sep)`（`List<String>` のみ） | `String` |
| `map(f)` | `List<U>` |
| `filter(f)` | `List<T>` |
| `fold(init, f)` | `U`（`f` は `fn(U, T) -> U`） |
| `any(f)`, `all(f)` | `Bool` |

**Map\<K, V\>**

| メソッド | 戻り値 |
| --- | --- |
| `len()`, `is_empty()`, `has(key)` | `Int`, `Bool`, `Bool` |
| `get(key)` | `Option<V>` |
| `set(key, value)`, `remove(key)` | `Map<K, V>`（新しいMap） |
| `keys()`, `values()` | `List<K>`, `List<V>`（キーの昇順） |

**数値**

| 対象 | メソッド |
| --- | --- |
| `Int` | `to_float()`, `abs()`, `min(x)`, `max(x)` |
| `Float` | `to_int()`（切り捨て）, `abs()`, `sqrt()`, `floor()`, `ceil()`, `round()`, `min(x)`, `max(x)` |

**Option / Result**

| 対象 | メソッド |
| --- | --- |
| `Option<T>` | `unwrap_or(default)`, `is_some()`, `is_none()` |
| `Result<T, E>` | `unwrap_or(default)`, `is_ok()`, `is_err()` |

## 9. テスト

```
test "divide" {
  assert divide(6, 2) == Ok(3)
  assert divide(1, 0) == Err("division by zero")
}
```

`test` ブロックは `yui test` で実行される。比較の `assert` が失敗すると、左右の値が表示される。
入出力を行うテストは `test "name" uses io { ... }` と書く。

## 10. ツール

```
yui check   <file> [--json]              型チェック（import したモジュールも含む）
yui run     <file> [--json] [-- 引数...]  main を実行
yui test    <file> [--json]              test ブロックを実行
yui fmt     <file> [--check]             整形して上書き（--check は確認のみ）
yui build   <file> [-o <出力>]            単体の実行ファイルを作る
yui outline <file>                       宣言のシグネチャだけを表示
yui --version                            バージョンを表示

--root <dir>   import の基準フォルダ（既定: <file> のあるフォルダ）
```

- `yui run` は `fn main()`（引数なし、戻り値 `Unit`）を実行する。`--` の後ろの引数は `args()` で受け取る。
- `--json` を付けると、診断やテスト結果を1個のJSONとして標準出力に出す。
  `yui run` だけは標準出力をプログラム自身の出力に使うので、診断のJSONは標準エラーに出す。
- 失敗時の終了コードは 1。`yui fmt --check` は整形が必要なら 1。

### 単体の実行ファイル（`yui build`）

`yui build main.yui` は、型チェックに通ったプログラムを1個の実行ファイルにする
（既定の出力先はソースと同じ場所の `main`、Windows では `main.exe`）。

- 中身は `yui` 本体の複製に、プログラムのソース（import したモジュールを含む）を付け足したもの。
  ほかのファイルも `yui` のインストールも不要で、どこへ移しても動く。
- 起動すると `main` を実行する。コマンドライン引数はすべてプログラムのもので、`args()` で受け取る。
- 実行時エラーの表示に出るファイル名は、ビルドした環境のパスではなくプログラム内の相対名になる。
- 大きさは `yui` 本体とほぼ同じ（約0.9MB）。ソースはそのまま入るので、取り出して読むことができる。
- macOS では作成後に `codesign` でアドホック署名をし直す。

### 整形（`yui fmt`）

- インデントは2スペース。1行は100桁まで。
- 収まれば1行に書き、収まらなければ要素ごとに改行して末尾にカンマを付ける。
- ブロックが式1つだけの `if` は、収まれば1行にする（`if x < 0 { return None }`）。
- `match` と関数本体は常に複数行。不要な括弧は取り除く。
- コメントと、文の間の空行（1行まで）は保持する。宣言の間は空行1行にそろえる。
- 文字列リテラルと数値リテラルは書かれたまま保つ。
- 式の途中に書いたコメントは、その文の次の行に移動することがある。

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
| `E02xx` | 名前解決（未定義、重複、モジュールが見つからない） |
| `E03xx` | 型エラー（不一致、網羅性、到達不能な腕、未使用の値など） |
| `E04xx` | 副作用エラー |
| `E05xx` | 実行時エラー（assert、契約、オーバーフロー、範囲外など） |
| `E09xx` | ツール内部のエラー |

## 11. 実装上の制限

- 実行はバイトコードにコンパイルしてから行う。x86-64 ではそれを機械語に変換して実行し（JIT）、
  それ以外の CPU では仮想マシンで実行する。関数呼び出しの入れ子は 10,000 段まで。
- 副作用を宣言していない関数は、同じ引数なら必ず同じ結果になる。JIT はこれを利用して、引数と戻り値が
  `Int`（引数は3個まで）で自分自身を2回以上呼ぶ関数の結果を、小さな表に記録して再利用する。
  結果は変わらず、`fib` のような再帰が速くなる。表は関数ごとに1,024件の固定サイズ。
- ユーザー定義型にメソッドは定義できない（関数として書く）。
- 型パラメータにトレイト境界はない。ユーザー定義の高階関数は、効果について多相にできない
  （`fn(T) uses io` を受け取る関数と、効果のない関数を受け取る関数は別に書く）。
- `yui fmt` は長い二項演算やメソッドチェーンを途中で改行しない。

## 12. 今後の候補

- バイトコードVM / WASM出力（実行速度と配布先の拡大）
- ユーザー定義型のメソッドとトレイト
- パッケージ管理
