void :=
list := $(void) foo bar baz #

a := $(word $(words $(list)),$(list))
b := $(lastword $(list))

.PHONY: all

all: ; @test "$a" = "$b" && echo $a
