a = a
b := b
define c
c
endef

$(info $(flavor a) $(flavor b) $(flavor c))

n := b

undefine a
undefine $n
undefine c

$(info $(flavor a) $(flavor b) $(flavor c))


all: ;@:
