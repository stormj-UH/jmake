undefine = undefine

$(undefine) : ;@echo $@

%:undefine

all: undefine foo

%.x : undefine

foo:;
