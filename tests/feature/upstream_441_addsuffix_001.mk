string := $(addsuffix .c,srca.b.z.foo hacks)
one: ; @echo $(string)

two: ; @echo $(addsuffix foo,)
