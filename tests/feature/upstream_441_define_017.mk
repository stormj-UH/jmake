define = define

$(define) : ;@echo $@

%:define

all: define foo

%.x : define

foo:;
