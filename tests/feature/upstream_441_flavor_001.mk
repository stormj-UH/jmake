s := s
r = r

$(info u $(flavor u))
$(info s $(flavor s))
$(info r $(flavor r))

ra += ra
rc ?= rc

$(info ra $(flavor ra))
$(info rc $(flavor rc))

s += s
r += r

$(info s $(flavor s))
$(info r $(flavor r))


.PHONY: all
all:;@:
