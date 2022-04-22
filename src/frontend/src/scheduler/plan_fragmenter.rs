// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Formatter};
use std::sync::Arc;

use risingwave_common::error::Result;
use risingwave_pb::plan::plan_node::NodeBody;
use risingwave_pb::plan::{ExchangeInfo, Field as FieldProst};
use uuid::Uuid;

use crate::optimizer::plan_node::{PlanNodeId, PlanNodeType};
use crate::optimizer::property::Distribution;
use crate::optimizer::PlanRef;
use crate::scheduler::worker_node_manager::WorkerNodeManagerRef;

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct QueryId {
    pub id: String,
}

pub(crate) type StageId = u32;

/// Generated by `BatchPlanFragmenter` and used in query execution graph.
#[derive(Clone, Debug)]
pub struct ExecutionPlanNode {
    pub plan_node_id: PlanNodeId,
    pub plan_node_type: PlanNodeType,
    pub node: NodeBody,
    pub schema: Vec<FieldProst>,

    pub children: Vec<Arc<ExecutionPlanNode>>,

    /// If this node is `BatchExchange`, we need this so that when creating `PlanNode` we can find
    /// `ExchangeSource` from scheduler.
    ///
    /// `None` when this node is not `BatchExchange`.
    pub stage_id: Option<StageId>,
}

impl From<PlanRef> for ExecutionPlanNode {
    fn from(plan_node: PlanRef) -> Self {
        Self {
            plan_node_id: plan_node.plan_base().id,
            plan_node_type: plan_node.node_type(),
            node: plan_node.to_batch_prost_body(),
            children: vec![],
            schema: plan_node.schema().to_prost(),
            stage_id: None,
        }
    }
}

impl ExecutionPlanNode {
    pub fn node_type(&self) -> PlanNodeType {
        self.plan_node_type
    }
}

/// `BatchPlanFragmenter` splits a query plan into fragments.
pub struct BatchPlanFragmenter {
    query_id: QueryId,
    stage_graph_builder: StageGraphBuilder,
    next_stage_id: u32,
    worker_node_manager: WorkerNodeManagerRef,
}

impl Default for QueryId {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
        }
    }
}

impl BatchPlanFragmenter {
    pub fn new(worker_node_manager: WorkerNodeManagerRef) -> Self {
        Self {
            query_id: Default::default(),
            stage_graph_builder: StageGraphBuilder::new(),
            next_stage_id: 0,
            worker_node_manager,
        }
    }
}

/// Contains the connection info of each stage.
#[derive(Debug)]
pub struct Query {
    /// Query id should always be unique.
    pub(crate) query_id: QueryId,
    pub(crate) stage_graph: StageGraph,
}

impl Query {
    pub fn leaf_stages(&self) -> Vec<StageId> {
        let mut ret_leaf_stages = Vec::new();
        for stage_id in self.stage_graph.stages.keys() {
            if self
                .stage_graph
                .get_child_stages_unchecked(stage_id)
                .is_empty()
            {
                ret_leaf_stages.push(*stage_id);
            }
        }
        ret_leaf_stages
    }

    pub fn get_parents(&self, stage_id: &StageId) -> &HashSet<StageId> {
        self.stage_graph.parent_edges.get(stage_id).unwrap()
    }

    pub fn root_stage_id(&self) -> StageId {
        self.stage_graph.root_stage_id
    }
}

/// Fragment part of `Query`.
pub struct QueryStage {
    pub query_id: QueryId,
    pub id: StageId,
    pub root: Arc<ExecutionPlanNode>,
    pub exchange_info: ExchangeInfo,
    pub parallelism: u32,
}

impl Debug for QueryStage {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryStage")
            .field("id", &self.id)
            .field("parallelism", &self.parallelism)
            .field("exchange_info", &self.exchange_info)
            .finish()
    }
}

pub(crate) type QueryStageRef = Arc<QueryStage>;

struct QueryStageBuilder {
    query_id: QueryId,
    id: StageId,
    plan_root: PlanRef,
    root: Option<Arc<ExecutionPlanNode>>,
    parallelism: u32,
    parent_parallelism: Option<u32>,
    exchange_info: ExchangeInfo,

    children_stages: Vec<QueryStageRef>,
}

impl QueryStageBuilder {
    fn new(
        root: PlanRef,
        id: StageId,
        query_id: QueryId,
        parallelism: u32,
        parent_parallelism: Option<u32>,
        exchange_info: ExchangeInfo,
    ) -> Self {
        Self {
            query_id,
            id,
            plan_root: root.clone(),
            root: None,
            parallelism,
            parent_parallelism,
            exchange_info,
            children_stages: vec![],
        }
    }

    fn finish(self, stage_graph_builder: &mut StageGraphBuilder) -> QueryStageRef {
        let stage = Arc::new(QueryStage {
            query_id: self.query_id,
            id: self.id,
            root: self.root.unwrap(),
            exchange_info: self.exchange_info,
            parallelism: self.parallelism,
        });

        stage_graph_builder.add_node(stage.clone());
        for child_stage in self.children_stages {
            stage_graph_builder.link_to_child(self.id, child_stage.id);
        }
        stage
    }
}

/// Maintains how each stage are connected.
#[derive(Debug)]
pub(crate) struct StageGraph {
    pub(crate) root_stage_id: StageId,
    pub stages: HashMap<StageId, QueryStageRef>,
    /// Traverse from top to down. Used in split plan into stages.
    child_edges: HashMap<StageId, HashSet<StageId>>,
    /// Traverse from down to top. Used in schedule each stage.
    parent_edges: HashMap<StageId, HashSet<StageId>>,
}

impl StageGraph {
    pub fn get_stage_unchecked(&self, stage_id: &StageId) -> QueryStageRef {
        self.stages.get(stage_id).unwrap().clone()
    }

    pub fn get_child_stages_unchecked(&self, stage_id: &StageId) -> &HashSet<StageId> {
        self.child_edges.get(stage_id).unwrap()
    }

    /// Returns stage ids in topology order, e.g. child stage always appears before parent.
    pub fn stage_ids_by_topo_order(&self) -> impl Iterator<Item = StageId> {
        let mut stack = Vec::with_capacity(self.stages.len());
        stack.push(self.root_stage_id);
        let mut ret = Vec::with_capacity(self.stages.len());
        let mut existing = HashSet::with_capacity(self.stages.len());

        while let Some(s) = stack.pop() {
            if !existing.contains(&s) {
                ret.push(s);
                existing.insert(s);
                stack.extend(&self.child_edges[&s]);
            }
        }

        ret.into_iter().rev()
    }
}

struct StageGraphBuilder {
    stages: HashMap<StageId, QueryStageRef>,
    child_edges: HashMap<StageId, HashSet<StageId>>,
    parent_edges: HashMap<StageId, HashSet<StageId>>,
}

impl StageGraphBuilder {
    pub fn new() -> Self {
        Self {
            stages: HashMap::new(),
            child_edges: HashMap::new(),
            parent_edges: HashMap::new(),
        }
    }

    pub fn build(self, root_stage_id: StageId) -> StageGraph {
        StageGraph {
            root_stage_id,
            stages: self.stages,
            child_edges: self.child_edges,
            parent_edges: self.parent_edges,
        }
    }

    /// Link parent stage and child stage. Maintain the mappings of parent -> child and child ->
    /// parent.
    ///
    /// # Arguments
    ///
    /// * `exchange_id` - The operator id of exchange executor.
    pub fn link_to_child(&mut self, parent_id: StageId, child_id: StageId) {
        self.child_edges
            .get_mut(&parent_id)
            .unwrap()
            .insert(child_id);
        self.parent_edges
            .get_mut(&child_id)
            .unwrap()
            .insert(parent_id);
    }

    pub fn add_node(&mut self, stage: QueryStageRef) {
        // Insert here so that left/root stages also has linkage.
        self.child_edges.insert(stage.id, HashSet::new());
        self.parent_edges.insert(stage.id, HashSet::new());
        self.stages.insert(stage.id, stage);
    }
}

impl BatchPlanFragmenter {
    /// Split the plan node into each stages, based on exchange node.
    pub fn split(mut self, batch_node: PlanRef) -> Result<Query> {
        let root_stage = self.new_stage(batch_node.clone(), None, None);
        let stage_graph = self.stage_graph_builder.build(root_stage.id);
        Ok(Query {
            stage_graph,
            query_id: self.query_id,
        })
    }

    fn new_stage(
        &mut self,
        root: PlanRef,
        parent_parallelism: Option<u32>,
        exchange_info: Option<ExchangeInfo>,
    ) -> QueryStageRef {
        let next_stage_id = self.next_stage_id;
        self.next_stage_id += 1;
        let parallelism = match parent_parallelism {
            // Non-root node
            Some(_) => self.worker_node_manager.worker_node_count(),
            // Root node.
            None => 1,
        };

        let exchange_info = match exchange_info {
            Some(info) => info,
            // Root stage, the exchange info should always be Single
            None => Distribution::Single.to_prost(1),
        };

        let mut builder = QueryStageBuilder::new(
            root.clone(),
            next_stage_id,
            self.query_id.clone(),
            parallelism as u32,
            parent_parallelism,
            exchange_info,
        );

        self.visit_node(root, &mut builder, None);

        builder.finish(&mut self.stage_graph_builder)
    }

    fn visit_node(
        &mut self,
        node: PlanRef,
        builder: &mut QueryStageBuilder,
        parent_exec_node: Option<&mut ExecutionPlanNode>,
    ) {
        match node.node_type() {
            PlanNodeType::BatchExchange => {
                self.visit_exchange(node.clone(), builder, parent_exec_node);
            }
            _ => {
                let mut execution_plan_node = ExecutionPlanNode::from(node.clone());

                for child in node.inputs() {
                    self.visit_node(child, builder, Some(&mut execution_plan_node));
                }

                if let Some(parent) = parent_exec_node {
                    parent.children.push(Arc::new(execution_plan_node));
                } else {
                    builder.root = Some(Arc::new(execution_plan_node));
                }
            }
        }
    }

    fn visit_exchange(
        &mut self,
        node: PlanRef,
        builder: &mut QueryStageBuilder,
        parent_exec_node: Option<&mut ExecutionPlanNode>,
    ) {
        let mut execution_plan_node = ExecutionPlanNode::from(node.clone());
        let child_exchange_info = Some(node.distribution().to_prost(builder.parallelism));
        let child_stage = self.new_stage(
            node.inputs()[0].clone(),
            Some(builder.parallelism),
            child_exchange_info,
        );
        execution_plan_node.stage_id = Some(child_stage.id);

        if let Some(parent) = parent_exec_node {
            parent.children.push(Arc::new(execution_plan_node));
        } else {
            builder.root = Some(Arc::new(execution_plan_node));
        }

        builder.children_stages.push(child_stage);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;
    use std::sync::Arc;

    use risingwave_common::catalog::{ColumnDesc, TableDesc};
    use risingwave_common::types::DataType;
    use risingwave_pb::common::{
        HostAddress, ParallelUnit, ParallelUnitType, WorkerNode, WorkerType,
    };
    use risingwave_pb::plan::plan_node::NodeBody;
    use risingwave_pb::plan::JoinType;

    use crate::optimizer::plan_node::{
        BatchExchange, BatchHashJoin, BatchSeqScan, EqJoinPredicate, LogicalJoin, LogicalScan,
        PlanNodeType,
    };
    use crate::optimizer::property::{Distribution, Order};
    use crate::optimizer::PlanRef;
    use crate::scheduler::plan_fragmenter::{BatchPlanFragmenter, StageId};
    use crate::scheduler::worker_node_manager::WorkerNodeManager;
    use crate::session::OptimizerContext;
    use crate::utils::Condition;

    #[tokio::test]
    async fn test_fragmenter() {
        // Construct a Hash Join with Exchange node.
        // Logical plan:
        //
        //    HashJoin
        //     /    \
        //   Scan  Scan
        //
        let ctx = OptimizerContext::mock().await;

        let batch_plan_node: PlanRef = BatchSeqScan::new(LogicalScan::new(
            "".to_string(),
            vec![0, 1],
            Rc::new(TableDesc {
                table_id: 0.into(),
                pk: vec![],
                columns: vec![
                    ColumnDesc {
                        data_type: DataType::Int32,
                        column_id: 0.into(),
                        name: "a".to_string(),
                        type_name: String::new(),
                        field_descs: vec![],
                    },
                    ColumnDesc {
                        data_type: DataType::Float64,
                        column_id: 1.into(),
                        name: "b".to_string(),
                        type_name: String::new(),
                        field_descs: vec![],
                    },
                ],
            }),
            ctx,
        ))
        .into();
        let batch_exchange_node1: PlanRef = BatchExchange::new(
            batch_plan_node.clone(),
            Order::default(),
            Distribution::HashShard(vec![0, 1]),
        )
        .into();
        let batch_exchange_node2: PlanRef = BatchExchange::new(
            batch_plan_node.clone(),
            Order::default(),
            Distribution::HashShard(vec![0, 1]),
        )
        .into();
        let hash_join_node: PlanRef = BatchHashJoin::new(
            LogicalJoin::new(
                batch_exchange_node1.clone(),
                batch_exchange_node2.clone(),
                JoinType::Inner,
                Condition::true_cond(),
            ),
            EqJoinPredicate::create(0, 0, Condition::true_cond()),
        )
        .into();
        let batch_exchange_node3: PlanRef = BatchExchange::new(
            hash_join_node.clone(),
            Order::default(),
            Distribution::Single,
        )
        .into();

        let worker1 = WorkerNode {
            id: 0,
            r#type: WorkerType::ComputeNode as i32,
            host: Some(HostAddress {
                host: "127.0.0.1".to_string(),
                port: 5687,
            }),
            state: risingwave_pb::common::worker_node::State::Running as i32,
            parallel_units: generate_parallel_units(0, 0),
        };
        let worker2 = WorkerNode {
            id: 1,
            r#type: WorkerType::ComputeNode as i32,
            host: Some(HostAddress {
                host: "127.0.0.1".to_string(),
                port: 5688,
            }),
            state: risingwave_pb::common::worker_node::State::Running as i32,
            parallel_units: generate_parallel_units(8, 1),
        };
        let worker3 = WorkerNode {
            id: 2,
            r#type: WorkerType::ComputeNode as i32,
            host: Some(HostAddress {
                host: "127.0.0.1".to_string(),
                port: 5689,
            }),
            state: risingwave_pb::common::worker_node::State::Running as i32,
            parallel_units: generate_parallel_units(16, 2),
        };
        let workers = vec![worker1, worker2, worker3];
        let worker_node_manager = Arc::new(WorkerNodeManager::mock(workers));
        // Break the plan node into fragments.
        let fragmenter = BatchPlanFragmenter::new(worker_node_manager);
        let query = fragmenter.split(batch_exchange_node3.clone()).unwrap();

        assert_eq!(query.stage_graph.root_stage_id, 0);
        assert_eq!(query.stage_graph.stages.len(), 4);

        // Check the mappings of child edges.
        assert_eq!(query.stage_graph.child_edges[&0], [1].into());
        assert_eq!(query.stage_graph.child_edges[&1], [2, 3].into());
        assert_eq!(query.stage_graph.child_edges[&2], HashSet::new());
        assert_eq!(query.stage_graph.child_edges[&3], HashSet::new());

        // Check the mappings of parent edges.
        assert_eq!(query.stage_graph.parent_edges[&0], HashSet::new());
        assert_eq!(query.stage_graph.parent_edges[&1], [0].into());
        assert_eq!(query.stage_graph.parent_edges[&2], [1].into());
        assert_eq!(query.stage_graph.parent_edges[&3], [1].into());

        // Verify topology order
        {
            let stage_id_to_pos: HashMap<StageId, usize> = query
                .stage_graph
                .stage_ids_by_topo_order()
                .enumerate()
                .map(|(pos, stage_id)| (stage_id, pos))
                .collect();

            for stage_id in query.stage_graph.stages.keys() {
                let stage_pos = stage_id_to_pos[stage_id];
                for child_stage_id in &query.stage_graph.child_edges[stage_id] {
                    let child_pos = stage_id_to_pos[child_stage_id];
                    assert!(stage_pos > child_pos);
                }
            }
        }

        // Check plan node in each stages.
        let root_exchange = query.stage_graph.stages.get(&0).unwrap();
        assert_eq!(root_exchange.root.node_type(), PlanNodeType::BatchExchange);
        assert_eq!(root_exchange.root.stage_id, Some(1));
        assert!(matches!(root_exchange.root.node, NodeBody::Exchange(_)));
        assert_eq!(root_exchange.parallelism, 1);

        let join_node = query.stage_graph.stages.get(&1).unwrap();
        assert_eq!(join_node.root.node_type(), PlanNodeType::BatchHashJoin);
        assert_eq!(join_node.parallelism, 3);

        assert!(matches!(join_node.root.node, NodeBody::HashJoin(_)));
        assert_eq!(join_node.root.stage_id, None);
        assert_eq!(2, join_node.root.children.len());

        assert!(matches!(
            join_node.root.children[0].node,
            NodeBody::Exchange(_)
        ));
        assert_eq!(join_node.root.children[0].stage_id, Some(2));
        assert_eq!(0, join_node.root.children[0].children.len());

        assert!(matches!(
            join_node.root.children[1].node,
            NodeBody::Exchange(_)
        ));
        assert_eq!(join_node.root.children[1].stage_id, Some(3));
        assert_eq!(0, join_node.root.children[1].children.len());

        let scan_node1 = query.stage_graph.stages.get(&2).unwrap();
        assert_eq!(scan_node1.root.node_type(), PlanNodeType::BatchSeqScan);
        assert_eq!(scan_node1.root.stage_id, None);
        assert_eq!(0, scan_node1.root.children.len());
        let scan_node2 = query.stage_graph.stages.get(&3).unwrap();
        assert_eq!(scan_node2.root.node_type(), PlanNodeType::BatchSeqScan);
        assert_eq!(scan_node2.root.stage_id, None);
        assert_eq!(0, scan_node2.root.children.len());
    }

    fn generate_parallel_units(start_id: u32, node_id: u32) -> Vec<ParallelUnit> {
        let parallel_degree = 8;
        let mut parallel_units = vec![ParallelUnit {
            id: start_id,
            r#type: ParallelUnitType::Single as i32,
            worker_node_id: node_id,
        }];
        for id in start_id + 1..start_id + parallel_degree {
            parallel_units.push(ParallelUnit {
                id,
                r#type: ParallelUnitType::Hash as i32,
                worker_node_id: node_id,
            });
        }
        parallel_units
    }
}
