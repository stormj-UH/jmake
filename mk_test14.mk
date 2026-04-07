.SECONDEXPANSION:
dep:=.x
all: hello.z
%.z: %$$(dep) ; @echo $@
%.x: ;
