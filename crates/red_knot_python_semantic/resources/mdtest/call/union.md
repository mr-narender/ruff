# Unions in calls

## Union of return types

```py
def bool_instance() -> bool:
    return True

flag = bool_instance()

if flag:

    def f() -> int:
        return 1

else:

    def f() -> str:
        return "foo"

reveal_type(f())  # revealed: int | str
```

## Calling with an unknown union

```py
from nonexistent import f  # error: [unresolved-import] "Cannot resolve import `nonexistent`"

def bool_instance() -> bool:
    return True

flag = bool_instance()

if flag:

    def f() -> int:
        return 1

reveal_type(f())  # revealed: Unknown | int
```

## Non-callable elements in a union

Calling a union with a non-callable element should emit a diagnostic.

```py
def bool_instance() -> bool:
    return True

flag = bool_instance()

if flag:
    f = 1
else:

    def f() -> int:
        return 1

x = f()  # error: "Object of type `Literal[1] | Literal[f]` is not callable (due to union element `Literal[1]`)"
reveal_type(x)  # revealed: Unknown | int
```

## Multiple non-callable elements in a union

Calling a union with multiple non-callable elements should mention all of them in the diagnostic.

```py
def bool_instance() -> bool:
    return True

flag, flag2 = bool_instance(), bool_instance()

if flag:
    f = 1
elif flag2:
    f = "foo"
else:

    def f() -> int:
        return 1

# error: "Object of type `Literal[1] | Literal["foo"] | Literal[f]` is not callable (due to union elements Literal[1], Literal["foo"])"
# revealed: Unknown | int
reveal_type(f())
```

## All non-callable union elements

Calling a union with no callable elements can emit a simpler diagnostic.

```py
def bool_instance() -> bool:
    return True

flag = bool_instance()

if flag:
    f = 1
else:
    f = "foo"

x = f()  # error: "Object of type `Literal[1] | Literal["foo"]` is not callable"
reveal_type(x)  # revealed: Unknown
```
