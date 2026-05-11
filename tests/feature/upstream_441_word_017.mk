void :=
list := $(void) foo bar baz #

a := $(word 1,$(list))
b := $(firstword $(list))

.PHONY: all

all: ; @test "$a" = "$b" && echo $a
