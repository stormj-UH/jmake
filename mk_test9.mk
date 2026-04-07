
.SECONDEXPANSION:
sim_base_rgg := just_a_name
sim_base_src := a
sim_base_f := a a a
sim_%.f: ${sim_$$*_f} ; echo $@
sim_%.src: ${sim_$$*_src} ; echo $@
sim_%: \
        $(if $(sim_$$*_src),sim_%.src) \
        $(if $(sim_$$*_f),sim_%.f) \
        $(if $(sim_$$*_rgg),$(sim_$$*_rgg).s) ; echo $@
