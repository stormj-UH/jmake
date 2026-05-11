X = $(filter foo\\\%bar,foo\%bar foo\Xbar)
all:;@echo '$(X)'