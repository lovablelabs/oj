"use strict"; return ((function() {
  const { core, internals } = __bootstrap;
  const { EventEmitter } = core.loadExtScript("ext:deno_node/_events.mjs");
  const { init: initPrimary } = core.loadExtScript("ext:deno_node/internal/cluster/primary.ts");
  const { init: initChild } = core.loadExtScript("ext:deno_node/internal/cluster/child.ts");
  const cluster = new EventEmitter();
  initPrimary(cluster);
  internals.__initCluster = (uniqueId, schedPolicyEnv)=>{
    if (typeof uniqueId !== "string" || uniqueId.length === 0) {
      return;
    }
    initChild(cluster);
    if (typeof schedPolicyEnv === "string" && schedPolicyEnv.length > 0) {
      if (schedPolicyEnv === "rr") {
        cluster.schedulingPolicy = cluster.SCHED_RR;
      } else if (schedPolicyEnv === "none") {
        cluster.schedulingPolicy = cluster.SCHED_NONE;
      } else {
        const n = Number(schedPolicyEnv);
        if (!Number.isNaN(n)) {
          cluster.schedulingPolicy = n;
        }
      }
    }
    cluster._setupWorker();
  };
  // Use getters so consumers see the current value of `cluster.*` even after
  // `__initCluster` has flipped the EventEmitter to the child side.
  return {
    default: cluster,
    Worker: cluster.Worker,
    SCHED_NONE: cluster.SCHED_NONE,
    SCHED_RR: cluster.SCHED_RR,
    get isPrimary () {
      return cluster.isPrimary;
    },
    get isMaster () {
      return cluster.isMaster;
    },
    get isWorker () {
      return cluster.isWorker;
    },
    get workers () {
      return cluster.workers;
    },
    get settings () {
      return cluster.settings;
    },
    get schedulingPolicy () {
      return cluster.schedulingPolicy;
    },
    get fork () {
      return cluster.fork;
    },
    get disconnect () {
      return cluster.disconnect;
    },
    get setupPrimary () {
      return cluster.setupPrimary;
    },
    get setupMaster () {
      return cluster.setupMaster;
    },
    get worker () {
      return cluster.worker;
    }
  };
})());