.SECONDEXPANSION:
dep:=test.x
all: hello.z
%.z: %.x $$(dep) ; @echo $@
%.x: ;
