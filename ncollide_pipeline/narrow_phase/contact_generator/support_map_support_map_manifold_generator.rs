use std::marker::PhantomData;
use std::cell::RefCell;
use approx::ApproxEq;
use alga::linear::FiniteDimInnerSpace;

use na::{self, Id, Point2, Real, Unit};
use math::{Isometry, Point};
use utils::{self, IdAllocator};
use geometry::shape::{AnnotatedPoint, FeatureId, Segment, SegmentPointLocation, Shape, SupportMap};
use geometry::query::algorithms::Simplex;
use geometry::query::algorithms::gjk::GJKResult;
use geometry::query::ray_internal;
use geometry::query::contacts_internal;
use geometry::query::closest_points_internal;
use geometry::query::{Contact, ContactKinematic, ContactManifold, ContactPrediction,
                      PointQueryWithLocation};
use geometry::shape::ConvexPolyface;
use narrow_phase::{ContactDispatcher, ContactGenerator};

#[derive(Clone)]
struct ClippingCache<N: Real> {
    poly1: Vec<Point2<N>>,
    poly2: Vec<Point2<N>>,
}


impl<N: Real> ClippingCache<N> {
    pub fn new() -> Self {
        ClippingCache {
            poly1: Vec::with_capacity(4),
            poly2: Vec::with_capacity(4),
        }
    }

    pub fn clear(&mut self) {
        self.poly1.clear();
        self.poly2.clear();
    }
}

/// Persistent contact manifold computation between two shapes having a support mapping function.
///
/// It is based on the GJK algorithm.  This detector generates only one contact point. For a full
/// manifold generation, see `IncrementalContactManifoldGenerator`.
#[derive(Clone)]
pub struct SupportMapSupportMapManifoldGenerator<P: Point, M, S> {
    simplex: S,
    last_gjk_dir: Option<P::Vector>,
    last_optimal_dir: Option<Unit<P::Vector>>,
    contact_manifold: ContactManifold<P>,
    clip_cache: ClippingCache<P::Real>,
    new_contacts: Vec<(Contact<P>, FeatureId, FeatureId)>,
    manifold1: ConvexPolyface<P>,
    manifold2: ConvexPolyface<P>,
    mat_type: PhantomData<M>, // FIXME: can we avoid this?
}

impl<P, M, S> SupportMapSupportMapManifoldGenerator<P, M, S>
where
    P: Point,
    M: Isometry<P>,
    S: Simplex<AnnotatedPoint<P>>,
{
    /// Creates a new persistant collision detector between two shapes with support mapping
    /// functions.
    ///
    /// It is initialized with a pre-created simplex.
    pub fn new(simplex: S) -> SupportMapSupportMapManifoldGenerator<P, M, S> {
        SupportMapSupportMapManifoldGenerator {
            simplex: simplex,
            last_gjk_dir: None,
            last_optimal_dir: None,
            contact_manifold: ContactManifold::new(),
            clip_cache: ClippingCache::new(),
            new_contacts: Vec::new(),
            manifold1: ConvexPolyface::new(),
            manifold2: ConvexPolyface::new(),
            mat_type: PhantomData,
        }
    }

    fn save_new_contacts_as_contact_manifold<G1: ?Sized, G2: ?Sized>(
        &mut self,
        m1: &M,
        g1: &G1,
        m2: &M,
        g2: &G2,
        ids: &mut IdAllocator,
    ) where
        G1: SupportMap<P, M>,
        G2: SupportMap<P, M>,
    {
        self.contact_manifold.save_cache_and_clear(ids);
        for (c, f1, f2) in self.new_contacts.drain(..) {
            let kinematic =
                Self::contact_kinematic(m1, &self.manifold1, f1, m2, &self.manifold2, f2);
            let local1 = m1.inverse_transform_point(&c.world1);
            let local2 = m2.inverse_transform_point(&c.world2);
            let n1 = g1.normal_cone(&local1, f1);
            let n2 = g2.normal_cone(&local2, f2);

            if self.contact_manifold
                .push(c, local1, local2, n1, n2, f1, f2, kinematic, ids)
            {
                NAVOID.with(|e| e.borrow_mut().1 += 1);
            }
        }
    }

    fn contact_kinematic(
        m1: &M,
        manifold1: &ConvexPolyface<P>,
        f1: FeatureId,
        m2: &M,
        manifold2: &ConvexPolyface<P>,
        f2: FeatureId,
    ) -> ContactKinematic<P::Vector> {
        if na::dimension::<P::Vector>() == 2 {
            match (f1, f2) {
                (FeatureId::Vertex(..), FeatureId::Vertex(..)) => ContactKinematic::PointPoint,
                (FeatureId::Vertex(..), FeatureId::Edge(..)) => ContactKinematic::PointPlane,
                (FeatureId::Edge(..), FeatureId::Vertex(..)) => ContactKinematic::PlanePoint,
                _ => ContactKinematic::PointPoint,
            }
        } else {
            match (f1, f2) {
                (FeatureId::Vertex(..), FeatureId::Vertex(..)) => ContactKinematic::PointPoint,
                (FeatureId::Vertex(..), FeatureId::Face(..)) => ContactKinematic::PointPlane,
                (FeatureId::Face(..), FeatureId::Vertex(..)) => ContactKinematic::PlanePoint,
                (FeatureId::Edge(..), FeatureId::Vertex(..)) => {
                    let e1 = manifold1.edge(f1).expect("Invalid edge id.");
                    let dir1 = m1.inverse_transform_vector(&e1.direction().unwrap().unwrap());
                    ContactKinematic::LinePoint(Unit::new_unchecked(dir1))
                }
                (FeatureId::Vertex(..), FeatureId::Edge(..)) => {
                    let e2 = manifold2.edge(f2).expect("Invalid edge id.");
                    let dir2 = m2.inverse_transform_vector(&e2.direction().unwrap().unwrap());
                    ContactKinematic::PointLine(Unit::new_unchecked(dir2))
                }
                (FeatureId::Edge(..), FeatureId::Edge(..)) => {
                    let e1 = manifold1.edge(f1).expect("Invalid edge id.");
                    let e2 = manifold2.edge(f2).expect("Invalid edge id.");
                    let dir1 = m1.inverse_transform_vector(&e1.direction().unwrap().unwrap());
                    let dir2 = m2.inverse_transform_vector(&e2.direction().unwrap().unwrap());
                    ContactKinematic::LineLine(Unit::new_unchecked(dir1), Unit::new_unchecked(dir2))
                }
                _ => ContactKinematic::PointPoint,
            }
        }
    }

    fn clip_polyfaces(&mut self, normal: Unit<P::Vector>) {
        if na::dimension::<P::Vector>() == 2 {
            if self.manifold1.vertices.len() <= 1 || self.manifold2.vertices.len() <= 1 {
                return;
            }
            // In 2D we always end up with two points.
            let mut ortho: P::Vector = na::zero();
            ortho[0] = -normal.as_ref()[1];
            ortho[1] = normal.as_ref()[0];

            let mut seg1 = Segment::new(self.manifold1.vertices[0], self.manifold1.vertices[1]);
            let mut seg2 = Segment::new(self.manifold2.vertices[0], self.manifold2.vertices[1]);

            let ref_pt = *seg1.a();
            let mut range1 = [
                na::dot(&(*seg1.a() - ref_pt), &ortho),
                na::dot(&(*seg1.b() - ref_pt), &ortho),
            ];
            let mut range2 = [
                na::dot(&(*seg2.a() - ref_pt), &ortho),
                na::dot(&(*seg2.b() - ref_pt), &ortho),
            ];
            let mut features1 = [self.manifold1.vertices_id[0], self.manifold1.vertices_id[1]];
            let mut features2 = [self.manifold2.vertices_id[0], self.manifold2.vertices_id[1]];

            if range1[1] < range1[0] {
                range1.swap(0, 1);
                features1.swap(0, 1);
                seg1.swap();
            }

            if range2[1] < range2[0] {
                range2.swap(0, 1);
                features2.swap(0, 1);
                seg2.swap();
            }

            if range2[0] > range1[1] || range1[0] > range2[1] {
                return;
            }

            let _1: P::Real = na::one();
            let length1 = range1[1] - range1[0];
            let length2 = range2[1] - range2[0];

            if range2[0] > range1[0] {
                let bcoord = (range2[0] - range1[0]) / length1;
                let p1 = seg1.point_at(&SegmentPointLocation::OnEdge([_1 - bcoord, bcoord]));
                let p2 = *seg2.a();
                let contact = Contact::new_wo_depth(p1, p2, normal);

                self.new_contacts
                    .push((contact, self.manifold1.feature_id, features2[0]));
            } else {
                let bcoord = (range1[0] - range2[0]) / length2;
                let p1 = *seg1.a();
                let p2 = seg2.point_at(&SegmentPointLocation::OnEdge([_1 - bcoord, bcoord]));
                let contact = Contact::new_wo_depth(p1, p2, normal);

                self.new_contacts
                    .push((contact, features1[0], self.manifold2.feature_id));
            }

            if range2[1] < range1[1] {
                let bcoord = (range2[1] - range1[0]) / length1;
                let p1 = seg1.point_at(&SegmentPointLocation::OnEdge([_1 - bcoord, bcoord]));
                let p2 = *seg2.b();
                let contact = Contact::new_wo_depth(p1, p2, normal);

                self.new_contacts
                    .push((contact, self.manifold1.feature_id, features2[1]));
            } else {
                let bcoord = (range1[1] - range2[0]) / length2;
                let p1 = *seg1.b();
                let p2 = seg2.point_at(&SegmentPointLocation::OnEdge([_1 - bcoord, bcoord]));
                let contact = Contact::new_wo_depth(p1, p2, normal);

                self.new_contacts
                    .push((contact, features1[1], self.manifold2.feature_id));
            }
        } else {
            // FIXME: don't compute contacts further than the prediction.
            
            if self.manifold1.vertices.len() <= 2 && self.manifold2.vertices.len() <= 2 {
                return;
            }
            self.clip_cache.clear();

            // In 3D we may end up with more than two points.
            let mut basis = [na::zero(), na::zero()];
            let mut basis_i = 0;

            P::Vector::orthonormal_subspace_basis(&[normal.unwrap()], |dir| {
                basis[basis_i] = *dir;
                basis_i += 1;
                true
            });

            let ref_pt = self.manifold1.vertices[0];

            for pt in &self.manifold1.vertices {
                let dpt = *pt - ref_pt;
                let coords = Point2::new(na::dot(&basis[0], &dpt), na::dot(&basis[1], &dpt));
                self.clip_cache.poly1.push(coords);
            }

            for pt in &self.manifold2.vertices {
                let dpt = *pt - ref_pt;
                let coords = Point2::new(na::dot(&basis[0], &dpt), na::dot(&basis[1], &dpt));
                self.clip_cache.poly2.push(coords);
            }

            if self.clip_cache.poly2.len() > 2 {
                for i in 0 .. self.clip_cache.poly1.len() {
                    let pt = &self.clip_cache.poly1[i];

                    if utils::point_in_poly2d(pt, &self.clip_cache.poly2) {
                        let origin = ref_pt + basis[0] * pt.x + basis[1] * pt.y;

                        let n2 = self.manifold2.normal.as_ref().unwrap().unwrap();
                        let p2 = &self.manifold2.vertices[0];
                        let toi2 = ray_internal::plane_toi_with_line(p2, &n2, &origin, &normal.unwrap());
                        let world2 = origin + normal.unwrap() * toi2;
                        let world1 = self.manifold1.vertices[i];
                        let f2 = self.manifold2.feature_id;
                        let f1 = self.manifold1.vertices_id[i];
                        let contact = Contact::new_wo_depth(world1, world2, normal);                        
                        self.new_contacts.push((contact, f1, f2));
                    }
                }
            }

            if self.clip_cache.poly1.len() > 2 {
                for i in 0 .. self.clip_cache.poly2.len() {
                    let pt = &self.clip_cache.poly2[i];

                    if utils::point_in_poly2d(pt, &self.clip_cache.poly1) {
                        let origin = ref_pt + basis[0] * pt.x + basis[1] * pt.y;

                        let n1 = self.manifold1.normal.as_ref().unwrap().unwrap();
                        let p1 = &self.manifold1.vertices[0];
                        let toi1 = ray_internal::plane_toi_with_line(p1, &n1, &origin, &normal.unwrap());
                        let world1 = origin + normal.unwrap() * toi1;
                        let world2 = self.manifold2.vertices[i];
                        let f1 = self.manifold1.feature_id;
                        let f2 = self.manifold2.vertices_id[i];
                        let contact = Contact::new_wo_depth(world1, world2, normal);                        
                        self.new_contacts.push((contact, f1, f2));
                    }
                }
            }

            let nedges1 = self.manifold1.nedges();
            let nedges2 = self.manifold2.nedges();
            for i1 in 0..nedges1 {
                let j1 = (i1 + 1) % self.clip_cache.poly1.len();
                let seg1 = Segment::new(self.clip_cache.poly1[i1], self.clip_cache.poly1[j1]);

                for i2 in 0..nedges2 {
                    let j2 = (i2 + 1) % self.clip_cache.poly2.len();
                    let seg2 = Segment::new(self.clip_cache.poly2[i2], self.clip_cache.poly2[j2]);

                    if let (SegmentPointLocation::OnEdge(e1), SegmentPointLocation::OnEdge(e2)) =
                        closest_points_internal::segment_against_segment_with_locations(
                            &Id::new(),
                            &seg1,
                            &Id::new(),
                            &seg2,
                        ) {
                        let original1 = Segment::new(self.manifold1.vertices[i1], self.manifold1.vertices[j1]);
                        let original2 = Segment::new(self.manifold2.vertices[i2], self.manifold2.vertices[j2]);
                        let world1 = original1.point_at(&SegmentPointLocation::OnEdge(e1));
                        let world2 = original2.point_at(&SegmentPointLocation::OnEdge(e2));
                        let f1 = self.manifold1.edges_id[i1];
                        let f2 = self.manifold2.edges_id[i2];
                        let contact = Contact::new_wo_depth(world1, world2, normal);                        
                        self.new_contacts.push((contact, f1, f2));
                    }
                }
            }
        }
    }
}

thread_local! {
    pub static NAVOID: RefCell<(u32, u32)> = RefCell::new((0, 0));
}

impl<P, M, S> ContactGenerator<P, M> for SupportMapSupportMapManifoldGenerator<P, M, S>
where
    P: Point,
    M: Isometry<P>,
    S: Simplex<AnnotatedPoint<P>>,
{
    #[inline]
    fn update(
        &mut self,
        _: &ContactDispatcher<P, M>,
        ma: &M,
        a: &Shape<P, M>,
        mb: &M,
        b: &Shape<P, M>,
        prediction: &ContactPrediction<P::Real>,
        ids: &mut IdAllocator,
    ) -> bool {
        if let (Some(sma), Some(smb)) = (a.as_support_map(), b.as_support_map()) {
            /*
            if self.contain_optimal(ma, sma, mb, smb, prediction, ids) {
                self.save_new_contacts_as_contact_manifold(ma, mb, ids);
                NAVOID.with(|e| e.borrow_mut().0 += 1);
                return true;
            }*/

            let contact = contacts_internal::support_map_against_support_map_with_params(
                ma,
                sma,
                mb,
                smb,
                prediction.linear,
                &mut self.simplex,
                self.last_gjk_dir,
            );

            // Generate a contact manifold.
            self.new_contacts.clear();
            self.manifold1.clear();
            self.manifold2.clear();

            match contact {
                GJKResult::Projection(ref contact, dir) => {
                    self.last_gjk_dir = Some(dir.unwrap());

                    if contact.depth > na::zero() {
                        sma.support_face_toward(ma, &contact.normal, &mut self.manifold1);
                        smb.support_face_toward(mb, &-contact.normal, &mut self.manifold2);
                        self.clip_polyfaces(contact.normal);
                    } else {
                        sma.support_feature_toward(
                            ma,
                            &contact.normal,
                            prediction.angular1,
                            &mut self.manifold1,
                        );
                        smb.support_feature_toward(
                            mb,
                            &-contact.normal,
                            prediction.angular2,
                            &mut self.manifold2,
                        );
                        self.clip_polyfaces(contact.normal);
                    }

                    if self.new_contacts.len() == 0 {
                        self.new_contacts.push((
                            contact.clone(),
                            self.manifold1.feature_id,
                            self.manifold2.feature_id,
                        ));
                    }
                }
                GJKResult::NoIntersection(dir) => self.last_gjk_dir = Some(dir),
                _ => {}
            }

            self.save_new_contacts_as_contact_manifold(ma, sma, mb, smb, ids);

            true
        } else {
            false
        }
    }

    #[inline]
    fn num_contacts(&self) -> usize {
        self.contact_manifold.len()
    }

    #[inline]
    fn contacts<'a: 'b, 'b>(&'a self, out: &'b mut Vec<&'a ContactManifold<P>>) {
        if self.contact_manifold.len() != 0 {
            out.push(&self.contact_manifold)
        }
    }
}