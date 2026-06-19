ARG IMAGE=containers.intersystems.com/intersystems/irishealth:2026.1
FROM ${IMAGE}

# Build-time setup: unexpire dev passwords, set _SYSTEM=SYS (the gateway CSP.ini auth),
# and enable Interoperability on USER so the interop tools have a real production namespace.
# The license key is mounted at runtime (compose volume); build-time IRIS runs key-less for setup.
USER irisowner
COPY --chown=irisowner:irisowner iris.script /opt/irisbuild/iris.script
COPY --chown=irisowner:irisowner merge.cpf  /opt/irisbuild/merge.cpf
RUN iris start IRIS \
    && iris session IRIS < /opt/irisbuild/iris.script \
    && iris stop IRIS quietly
