.SECONDEXPANSION:
dep:=hello.x
all: hello.z
%.z: %.x; @echo $@
%.x: ;
unrelated: $$(dep)
