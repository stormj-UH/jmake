NEQ = $(subst $1,,$2)
f =
t = true

all:
	@echo 1 $(or    ,    )
	@echo 2 $(or $t)
	@echo 3 $(or ,$t)
	@echo 4 $(or z,true,$f,false)
	@echo 5 $(or $t,$(info bad short-circuit))
	@echo 6 $(or $(info short-circuit),$t)
	@echo 7 $(or $(call NEQ,a,b),true)
	@echo 8 $(or $(call NEQ,a,a),true)
	@echo 9 $(or z,true,fal,se) hi
	@echo 10 $(or ,true,fal,se)there
	@echo 11 $(or   $(e) ,$f)