null =
v = $(let    ,$(info blankvar),abc)
x = $(let $(null),$(info side-effect),abc)
y = $(let y,,$ydef)

all: ; @echo $v/$x/$y