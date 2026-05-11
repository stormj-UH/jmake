FOO = a b	cde     f
all: ; @echo $(words $(sort $(FOO)))
