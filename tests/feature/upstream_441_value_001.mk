export FOO = foo

recurse = FOO = $FOO
static := FOO = $(value FOO)

all: ; @echo $(recurse) $(value recurse) $(static) $(value static)
