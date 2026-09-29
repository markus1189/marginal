# Review: git diff HEAD

Select a hunk with `J`/`K`, or any run of lines inside one with `V` and
`j`/`k`. Comments come back anchored to the real file and line.

## `before.txt → after it.txt`

*(no content change)*

## `blob.bin`

`7989678..8121008`

*(binary — no line-level diff)*

## `deleted.txt (deleted)`

`286c5f5..0000000`

### @@ -1 +0,0 @@

````diff
-gone
````

## `empty.txt (new file)`

`0000000..e69de29`

*(no content change)*

## `noeol.txt`

`4e1f202..08a4c5e`

### @@ -1,2 +1,2 @@

````diff
 keep
-old
\ No newline at end of file
+new
\ No newline at end of file
````

## `notes.md`

`ed16eca..de25bf7`

### @@ -4,4 +4,4 @@

````diff
 echo hi
 ```
 
-end
+changed end
````

## `quo"te.txt`

`975fbec..9bda8c3`

### @@ -1 +1 @@

````diff
-y
+Y
````

## `script.sh`

mode `100644` → `100755`

*(no content change)*

## `sub`

`96375d8..0ad0e35`

### @@ -1 +1 @@

````diff
-Subproject commit 96375d8e9abcbf417ea8c30eafbb2ff6a98f0fc3
+Subproject commit 0ad0e357f9ff86eef5e776cabaf8701782b42b33
````

## `"tab\there.txt"`

`587be6b..62d8fe9`

### @@ -1 +1 @@

````diff
-x
+X
````

## `umlaut-ü.txt`

`8be8316..8315c96`

### @@ -1 +1 @@

````diff
-ä
+Ä
````

## `with space.txt`

`4cb29ea..ddc897f`

### @@ -1,3 +1,3 @@

````diff
 one
-two
+TWO
 three
````

