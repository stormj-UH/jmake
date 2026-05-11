NEQ = $(subst $1,,$2)
e =

all:
	@echo 1 $(if    ,true,false)
	@echo 2 $(if ,true,)
	@echo 3 $(if ,true)
	@echo 4 $(if z,true,false)
	@echo 5 $(if z,true,$(shell echo hi))
	@echo 6 $(if ,$(shell echo hi),false)
	@echo 7 $(if $(call NEQ,a,b),true,false)
	@echo 8 $(if $(call NEQ,a,a),true,false)
	@echo 9 $(if z,true,fal,se) hi
	@echo 10 $(if ,true,fal,se)there
	@echo 11 $(if $(e) ,true,false)
